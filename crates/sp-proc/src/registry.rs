//! The stage registry: kind → constructor plus descriptor
//! (`docs/DESIGN.md` §4.1).
//!
//! A saved pipeline names its stages by kind, so something has to turn
//! `"dsp.filter.biquad"` back into a stage. The registry is that something,
//! and it is the only place the orchestrator learns a stage exists — which is
//! the seam a plugin interface would slot into later.
//!
//! Each group gets its own instance of every stage: `process` takes `&mut
//! self`, so a stage may keep state across the groups *it* sees without the
//! scheduler having to serialise groups behind it.

use std::collections::BTreeMap;

use crate::stage::{Stage, StageDescriptor};

/// Builds a fresh instance of one stage.
pub type StageFactory = fn() -> Box<dyn Stage>;

/// A registered stage kind.
#[derive(Debug, Clone, Copy)]
pub struct Registration {
    pub descriptor: &'static StageDescriptor,
    pub factory: StageFactory,
}

/// A stage was declared in a way the rest of the system cannot work with.
/// Raised at registration, so a bad declaration fails at startup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("two stages are registered as '{0}'")]
    Duplicate(String),

    #[error("the stage '{0}' declares a repeated port or parameter name")]
    Inconsistent(String),
}

/// Every stage kind this build can run.
#[derive(Debug, Clone, Default)]
pub struct StageRegistry {
    entries: BTreeMap<&'static str, Registration>,
}

impl StageRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a stage by constructing one and reading its descriptor, so
    /// the kind can never disagree with the instance it makes.
    pub fn register(&mut self, factory: StageFactory) -> Result<(), RegistryError> {
        let descriptor = factory().descriptor();
        if !descriptor.is_consistent() {
            return Err(RegistryError::Inconsistent(descriptor.kind.to_owned()));
        }
        if self.entries.contains_key(descriptor.kind) {
            return Err(RegistryError::Duplicate(descriptor.kind.to_owned()));
        }
        self.entries.insert(
            descriptor.kind,
            Registration {
                descriptor,
                factory,
            },
        );
        Ok(())
    }

    /// Registers several stages, stopping at the first bad declaration.
    pub fn register_all(
        &mut self,
        factories: impl IntoIterator<Item = StageFactory>,
    ) -> Result<(), RegistryError> {
        for factory in factories {
            self.register(factory)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn get(&self, kind: &str) -> Option<Registration> {
        self.entries.get(kind).copied()
    }

    #[must_use]
    pub fn descriptor(&self, kind: &str) -> Option<&'static StageDescriptor> {
        self.entries.get(kind).map(|entry| entry.descriptor)
    }

    /// A fresh instance of one kind.
    #[must_use]
    pub fn create(&self, kind: &str) -> Option<Box<dyn Stage>> {
        self.entries.get(kind).map(|entry| (entry.factory)())
    }

    #[must_use]
    pub fn contains(&self, kind: &str) -> bool {
        self.entries.contains_key(kind)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every registered descriptor, in kind order — what the stage palette
    /// lists.
    pub fn descriptors(&self) -> impl Iterator<Item = &'static StageDescriptor> + '_ {
        self.entries.values().map(|entry| entry.descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ConfigError;
    use crate::frame::GroupFrame;
    use crate::param::ParamSet;
    use crate::stage::{PortKind, PortSpec, StageCtx, StageOutput};

    #[derive(Debug, Default)]
    struct Nothing {
        /// State, so two instances are two allocations rather than one
        /// zero-sized address.
        gain: f64,
    }

    static NOTHING: StageDescriptor = StageDescriptor::new("test.nothing", 1, "Nothing");

    impl Stage for Nothing {
        fn descriptor(&self) -> &'static StageDescriptor {
            &NOTHING
        }
        fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
            self.gain = params.f64_or("gain", 1.0);
            Ok(())
        }
        fn process(
            &mut self,
            _ctx: &StageCtx,
            input: &GroupFrame,
        ) -> Result<StageOutput, crate::error::StageError> {
            Ok(StageOutput::passthrough_of(input))
        }
    }

    #[derive(Debug, Default)]
    struct Muddled;

    const REPEATED: &[PortSpec] = &[
        PortSpec::required("signals", PortKind::ANY_SIGNALS),
        PortSpec::optional("signals", PortKind::Any),
    ];
    static MUDDLED: StageDescriptor =
        StageDescriptor::new("test.muddled", 1, "Muddled").reading(REPEATED);

    impl Stage for Muddled {
        fn descriptor(&self) -> &'static StageDescriptor {
            &MUDDLED
        }
        fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
            Ok(())
        }
        fn process(
            &mut self,
            _ctx: &StageCtx,
            input: &GroupFrame,
        ) -> Result<StageOutput, crate::error::StageError> {
            Ok(StageOutput::passthrough_of(input))
        }
    }

    fn nothing() -> Box<dyn Stage> {
        Box::<Nothing>::default()
    }

    fn muddled() -> Box<dyn Stage> {
        Box::<Muddled>::default()
    }

    #[test]
    fn a_registered_kind_creates_instances() {
        let mut registry = StageRegistry::new();
        registry.register(nothing).unwrap();
        assert!(registry.contains("test.nothing"));
        let stage = registry.create("test.nothing").unwrap();
        assert_eq!(stage.descriptor().version, 1);
        assert!(registry.create("test.absent").is_none());
    }

    #[test]
    fn each_call_hands_back_a_separate_instance() {
        // Groups run concurrently, so two of them must never share a stage's
        // mutable state.
        let mut registry = StageRegistry::new();
        registry.register(nothing).unwrap();
        let mut one = registry.create("test.nothing").unwrap();
        let two = registry.create("test.nothing").unwrap();
        one.configure(&ParamSet::new().with("gain", 4.0)).unwrap();
        assert!(
            !std::ptr::eq(std::ptr::from_ref(&*one), std::ptr::from_ref(&*two)),
            "two groups must never share a stage's mutable state"
        );
    }

    #[test]
    fn registering_a_kind_twice_is_refused() {
        let mut registry = StageRegistry::new();
        registry.register(nothing).unwrap();
        assert_eq!(
            registry.register(nothing).unwrap_err(),
            RegistryError::Duplicate("test.nothing".into())
        );
    }

    #[test]
    fn a_mis_declared_stage_fails_at_registration() {
        let mut registry = StageRegistry::new();
        assert_eq!(
            registry.register(muddled).unwrap_err(),
            RegistryError::Inconsistent("test.muddled".into())
        );
        assert!(registry.is_empty(), "nothing half-registered");
    }

    #[test]
    fn descriptors_list_in_kind_order() {
        let mut registry = StageRegistry::new();
        registry.register(nothing).unwrap();
        let kinds: Vec<_> = registry.descriptors().map(|d| d.kind).collect();
        assert_eq!(kinds, ["test.nothing"]);
    }
}
