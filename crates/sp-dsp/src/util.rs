//! Utility stages (`docs/DESIGN.md` §9.8).

use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::ParamSet;
use sp_proc::stage::{Stage, StageCtx, StageDescriptor, StageOutput};
use sp_proc::GroupFrame;

use crate::{SIGNALS_IN, SIGNALS_OUT};

static PASSTHROUGH: StageDescriptor =
    StageDescriptor::new("dsp.util.passthrough", 1, "Passthrough")
        .describing("Changes nothing; a labelled place to look at the signals.")
        .reading(SIGNALS_IN)
        .writing(SIGNALS_OUT);

/// A labelled inspection point.
///
/// It does nothing to the signals, which is the point: dropping one into a
/// pipeline records the group at that position, so "what did it look like
/// before the filter" is a stage on the rail rather than a rerun. It costs
/// almost nothing to keep — content addressing means its recorded signals
/// share the blobs they came in on (§5.3).
#[derive(Debug, Default)]
pub struct Passthrough;

impl Stage for Passthrough {
    fn descriptor(&self) -> &'static StageDescriptor {
        &PASSTHROUGH
    }

    fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
        Ok(())
    }

    fn process(&mut self, _ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        Ok(StageOutput::passthrough_of(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, process, values_of};

    #[test]
    fn a_passthrough_hands_every_signal_on_unchanged() {
        let mut stage = Passthrough;
        let input = frame(&[("a", vec![1.0, 2.0]), ("b", vec![3.0])]);
        let (next, output) = process(&mut stage, &input);
        assert_eq!(next.signals.len(), 2);
        assert_eq!(values_of(&next, 0), [1.0, 2.0]);
        assert_eq!(values_of(&next, 1), [3.0]);
        assert!(output.artifacts.is_empty());
        assert!(output.metrics.is_empty());
    }

    #[test]
    fn the_samples_keep_their_content_hash_so_recording_shares_the_blob() {
        let input = frame(&[("a", vec![1.0, 2.0])]);
        let mut stage = Passthrough;
        let (next, _) = process(&mut stage, &input);
        assert_eq!(
            next.signals[0].content_hash(),
            input.signals[0].content_hash()
        );
    }
}
