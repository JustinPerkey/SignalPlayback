//! Addressing and editing the node tree (`docs/DESIGN.md` §8.3).
//!
//! Every node has a JSON pointer into the serialised [`GenSpec`] — `/root`,
//! `/root/terms/1`, `/root/carrier`, `/root/parts/0/node`. One address serves
//! the tree editor's selection, the validator's issue list ([`crate::validate`])
//! and a sweep's target ([`crate::sweep`]), so a message about a parameter can
//! always be put next to the field that produced it.

use serde_json::Value;

use crate::spec::{ConcatPart, GenSpec, Node, NodeKind};

/// The pointer of the spec's root node.
pub const ROOT: &str = "/root";

/// Field names a child hangs off, matching the serialised spec.
pub const TERMS: &str = "terms";
pub const PARTS: &str = "parts";
pub const INPUT: &str = "input";
pub const CARRIER: &str = "carrier";
pub const MODULATOR: &str = "modulator";

/// The pointer of a child of `parent`. `index` is required for the
/// variable-arity fields and ignored for the rest.
#[must_use]
pub fn child_pointer(parent: &str, field: &str, index: Option<usize>) -> String {
    match (field, index) {
        (PARTS, Some(index)) => format!("{parent}/{PARTS}/{index}/node"),
        (_, Some(index)) => format!("{parent}/{field}/{index}"),
        (_, None) => format!("{parent}/{field}"),
    }
}

/// The pointer of `pointer`'s parent node, or `None` for the root.
#[must_use]
pub fn parent_pointer(pointer: &str) -> Option<&str> {
    let trimmed = pointer.strip_suffix("/node").unwrap_or(pointer);
    let cut = trimmed.rfind('/')?;
    let head = &trimmed[..cut];
    // A `/terms/1` or `/parts/1` tail eats two segments.
    let head = match head.rfind('/') {
        Some(prior) if matches!(&head[prior + 1..], TERMS | PARTS) => &head[..prior],
        _ => head,
    };
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

/// A copy of `spec` with the value at `json_pointer` replaced.
///
/// The substitution goes through the serialised form rather than through a
/// match over [`Node`], so a parameter of a node variant added later is
/// editable and sweepable with no code here. A value the spec will not accept
/// back — a duty cycle in a frequency field, say — is reported rather than
/// applied, so the caller can keep the old spec and show the message.
pub fn set_json(spec: &GenSpec, json_pointer: &str, value: Value) -> Result<GenSpec, String> {
    let mut document = serde_json::to_value(spec)
        .map_err(|error| format!("the spec will not serialise: {error}"))?;
    let slot = document
        .pointer_mut(json_pointer)
        .ok_or_else(|| format!("'{json_pointer}' is not a field of this spec"))?;
    *slot = value;
    serde_json::from_value(document).map_err(|error| error.to_string())
}

/// The value at `json_pointer` in the serialised spec.
#[must_use]
pub fn get_json(spec: &GenSpec, json_pointer: &str) -> Option<Value> {
    serde_json::to_value(spec)
        .ok()?
        .pointer(json_pointer)
        .cloned()
}

/// One row of the tree editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRef {
    pub pointer: String,
    /// Nesting depth, for the row's indent. The root is zero.
    pub depth: usize,
    /// What the node is to its parent — `term 1`, `carrier`, `part 0`.
    pub slot: String,
    pub kind: NodeKind,
    /// The node's parameters in one line, for the row's subtitle.
    pub summary: String,
}

/// Every node in the tree, parents before children, as the editor lists them.
#[must_use]
pub fn outline(spec: &GenSpec) -> Vec<NodeRef> {
    let mut out = Vec::new();
    collect(&spec.root, ROOT.to_owned(), 0, "root".to_owned(), &mut out);
    out
}

fn collect(node: &Node, pointer: String, depth: usize, slot: String, out: &mut Vec<NodeRef>) {
    out.push(NodeRef {
        depth,
        slot,
        kind: node.kind(),
        summary: summary(node),
        pointer: pointer.clone(),
    });
    match node {
        Node::Sum { terms } | Node::Product { terms } => {
            for (index, term) in terms.iter().enumerate() {
                collect(
                    term,
                    child_pointer(&pointer, TERMS, Some(index)),
                    depth + 1,
                    format!("term {index}"),
                    out,
                );
            }
        }
        Node::Concat { parts } => {
            for (index, part) in parts.iter().enumerate() {
                collect(
                    &part.node,
                    child_pointer(&pointer, PARTS, Some(index)),
                    depth + 1,
                    format!("part {index} ({} s)", trim(part.duration_s)),
                    out,
                );
            }
        }
        Node::Gain { input, .. }
        | Node::Delay { input, .. }
        | Node::Clip { input, .. }
        | Node::Envelope { input, .. }
        | Node::Resample { input, .. } => collect(
            input,
            child_pointer(&pointer, INPUT, None),
            depth + 1,
            INPUT.to_owned(),
            out,
        ),
        Node::Modulate {
            carrier, modulator, ..
        } => {
            collect(
                carrier,
                child_pointer(&pointer, CARRIER, None),
                depth + 1,
                CARRIER.to_owned(),
                out,
            );
            collect(
                modulator,
                child_pointer(&pointer, MODULATOR, None),
                depth + 1,
                MODULATOR.to_owned(),
                out,
            );
        }
        _ => {}
    }
}

/// A node's parameters in one line.
#[must_use]
pub fn summary(node: &Node) -> String {
    match node {
        Node::Sine { freq_hz, amp, .. } => format!("{} @ {}", hz(*freq_hz), trim(*amp)),
        Node::Square {
            freq_hz, amp, duty, ..
        } => format!("{} @ {}, duty {}", hz(*freq_hz), trim(*amp), trim(*duty)),
        Node::Triangle { freq_hz, amp, .. } | Node::Sawtooth { freq_hz, amp, .. } => {
            format!("{} @ {}", hz(*freq_hz), trim(*amp))
        }
        Node::Pulse {
            period_s,
            width_s,
            amp,
            ..
        } => format!(
            "every {} s, {} s wide @ {}",
            trim(*period_s),
            trim(*width_s),
            trim(*amp)
        ),
        Node::Chirp {
            f0_hz,
            f1_hz,
            sweep,
            ..
        } => format!("{} to {}, {}", hz(*f0_hz), hz(*f1_hz), sweep.label()),
        Node::Dc { level } => trim(*level),
        Node::Ramp { start, end } => format!("{} to {}", trim(*start), trim(*end)),
        Node::Step { at_s, after, .. } => format!("to {} at {} s", trim(*after), trim(*at_s)),
        Node::Impulse { at_s, amp } => format!("{} at {} s", trim(*amp), trim(*at_s)),
        Node::Noise { kind, amp } => format!("{} @ {}", kind.label(), trim(*amp)),
        Node::Prbs { order, amp, .. } => format!("order {order} @ {}", trim(*amp)),
        Node::Expr { source } => source.clone(),
        Node::Sum { terms } => format!("{} term(s)", terms.len()),
        Node::Product { terms } => format!("{} factor(s)", terms.len()),
        Node::Concat { parts } => format!("{} part(s)", parts.len()),
        Node::Gain { factor, .. } => format!("x{}", trim(*factor)),
        Node::Delay { by_s, .. } => format!("{} s", trim(*by_s)),
        Node::Clip { lo, hi, .. } => format!("{} to {}", trim(*lo), trim(*hi)),
        Node::Envelope { env, .. } => env.label().to_owned(),
        Node::Modulate { kind, .. } => kind.label().to_owned(),
        Node::Resample { to_rate_hz, .. } => hz(*to_rate_hz),
        Node::FromSignal { signal_id } => format!("signal {}", signal_id.get()),
    }
}

/// A frequency with the prefix that keeps it readable.
fn hz(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1e9 {
        format!("{} GHz", trim(value / 1e9))
    } else if magnitude >= 1e6 {
        format!("{} MHz", trim(value / 1e6))
    } else if magnitude >= 1e3 {
        format!("{} kHz", trim(value / 1e3))
    } else {
        format!("{} Hz", trim(value))
    }
}

/// A number without a trailing `.0` and without scientific noise.
fn trim(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    if text.is_empty() || text == "-0" {
        "0".to_owned()
    } else {
        text
    }
}

/// The tokens of a pointer below `/root`, or `None` when the pointer does not
/// start there.
fn tokens(pointer: &str) -> Option<Vec<&str>> {
    let rest = pointer.strip_prefix(ROOT)?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let rest = rest.strip_prefix('/')?;
    Some(rest.split('/').collect())
}

/// The node at `pointer`.
#[must_use]
pub fn node_at<'a>(spec: &'a GenSpec, pointer: &str) -> Option<&'a Node> {
    let tokens = tokens(pointer)?;
    descend(&spec.root, &tokens)
}

fn descend<'a>(node: &'a Node, tokens: &[&str]) -> Option<&'a Node> {
    let Some((head, rest)) = tokens.split_first() else {
        return Some(node);
    };
    match (node, *head) {
        (Node::Sum { terms } | Node::Product { terms }, TERMS) => {
            let (index, rest) = index_of(rest)?;
            descend(terms.get(index)?, rest)
        }
        (Node::Concat { parts }, PARTS) => {
            let (index, rest) = index_of(rest)?;
            let rest = rest.split_first().filter(|(head, _)| **head == "node")?.1;
            descend(&parts.get(index)?.node, rest)
        }
        (
            Node::Gain { input, .. }
            | Node::Delay { input, .. }
            | Node::Clip { input, .. }
            | Node::Envelope { input, .. }
            | Node::Resample { input, .. },
            INPUT,
        ) => descend(input, rest),
        (Node::Modulate { carrier, .. }, CARRIER) => descend(carrier, rest),
        (Node::Modulate { modulator, .. }, MODULATOR) => descend(modulator, rest),
        _ => None,
    }
}

/// The node at `pointer`, for editing.
pub fn node_at_mut<'a>(spec: &'a mut GenSpec, pointer: &str) -> Option<&'a mut Node> {
    let tokens = tokens(pointer)?;
    descend_mut(&mut spec.root, &tokens)
}

fn descend_mut<'a>(node: &'a mut Node, tokens: &[&str]) -> Option<&'a mut Node> {
    let Some((head, rest)) = tokens.split_first() else {
        return Some(node);
    };
    match (node, *head) {
        (Node::Sum { terms } | Node::Product { terms }, TERMS) => {
            let (index, rest) = index_of(rest)?;
            descend_mut(terms.get_mut(index)?, rest)
        }
        (Node::Concat { parts }, PARTS) => {
            let (index, rest) = index_of(rest)?;
            let rest = rest.split_first().filter(|(head, _)| **head == "node")?.1;
            descend_mut(&mut parts.get_mut(index)?.node, rest)
        }
        (
            Node::Gain { input, .. }
            | Node::Delay { input, .. }
            | Node::Clip { input, .. }
            | Node::Envelope { input, .. }
            | Node::Resample { input, .. },
            INPUT,
        ) => descend_mut(input, rest),
        (Node::Modulate { carrier, .. }, CARRIER) => descend_mut(carrier, rest),
        (Node::Modulate { modulator, .. }, MODULATOR) => descend_mut(modulator, rest),
        _ => None,
    }
}

fn index_of<'a>(tokens: &'a [&'a str]) -> Option<(usize, &'a [&'a str])> {
    let (head, rest) = tokens.split_first()?;
    Some((head.parse().ok()?, rest))
}

/// Replaces the node at `pointer`. Returns whether it was there.
pub fn replace(spec: &mut GenSpec, pointer: &str, node: Node) -> bool {
    match node_at_mut(spec, pointer) {
        Some(slot) => {
            *slot = node;
            true
        }
        None => false,
    }
}

/// Appends a child to a variable-arity node, returning its pointer.
pub fn add_child(spec: &mut GenSpec, pointer: &str) -> Option<String> {
    let node = node_at_mut(spec, pointer)?;
    match node {
        Node::Sum { terms } | Node::Product { terms } => {
            terms.push(Node::default());
            Some(child_pointer(pointer, TERMS, Some(terms.len() - 1)))
        }
        Node::Concat { parts } => {
            parts.push(ConcatPart {
                node: Node::default(),
                duration_s: 1.0,
            });
            Some(child_pointer(pointer, PARTS, Some(parts.len() - 1)))
        }
        _ => None,
    }
}

/// Removes the node at `pointer` from its parent, which must be a
/// variable-arity node with more than one child. Returns the pointer to select
/// afterwards.
pub fn remove(spec: &mut GenSpec, pointer: &str) -> Option<String> {
    let (parent_pointer, index) = slot_in_parent(pointer)?;
    let parent = node_at_mut(spec, &parent_pointer)?;
    match parent {
        Node::Sum { terms } | Node::Product { terms } if terms.len() > 1 => {
            terms.remove(index);
        }
        Node::Concat { parts } if parts.len() > 1 => {
            parts.remove(index);
        }
        _ => return None,
    }
    Some(parent_pointer)
}

/// Moves a child up (`-1`) or down (`1`) among its siblings, returning its new
/// pointer.
pub fn move_child(spec: &mut GenSpec, pointer: &str, delta: isize) -> Option<String> {
    let (parent_pointer, index) = slot_in_parent(pointer)?;
    let target = index.checked_add_signed(delta)?;
    let parent = node_at_mut(spec, &parent_pointer)?;
    let field = match parent {
        Node::Sum { terms } | Node::Product { terms } if target < terms.len() => {
            terms.swap(index, target);
            TERMS
        }
        Node::Concat { parts } if target < parts.len() => {
            parts.swap(index, target);
            PARTS
        }
        _ => return None,
    };
    Some(child_pointer(&parent_pointer, field, Some(target)))
}

/// A part's duration, for the Concat parameter form.
pub fn part_duration_mut<'a>(
    spec: &'a mut GenSpec,
    pointer: &str,
    index: usize,
) -> Option<&'a mut f64> {
    match node_at_mut(spec, pointer)? {
        Node::Concat { parts } => parts.get_mut(index).map(|part| &mut part.duration_s),
        _ => None,
    }
}

/// The parent pointer and the child's index, for a child in a variable-arity
/// slot.
fn slot_in_parent(pointer: &str) -> Option<(String, usize)> {
    let trimmed = pointer.strip_suffix("/node").unwrap_or(pointer);
    let cut = trimmed.rfind('/')?;
    let index: usize = trimmed[cut + 1..].parse().ok()?;
    let head = &trimmed[..cut];
    let field_cut = head.rfind('/')?;
    if !matches!(&head[field_cut + 1..], TERMS | PARTS) {
        return None;
    }
    Some((head[..field_cut].to_owned(), index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ModKind;

    fn nested() -> GenSpec {
        GenSpec::new(
            48_000.0,
            1.0,
            Node::Sum {
                terms: vec![
                    Node::Sine {
                        freq_hz: 100.0,
                        amp: 1.0,
                        phase_rad: 0.0,
                        offset: 0.0,
                    },
                    Node::Modulate {
                        carrier: Box::new(Node::Sine {
                            freq_hz: 2_000.0,
                            amp: 1.0,
                            phase_rad: 0.0,
                            offset: 0.0,
                        }),
                        modulator: Box::new(Node::Dc { level: 0.5 }),
                        kind: ModKind::Am { depth: 0.5 },
                    },
                    Node::Concat {
                        parts: vec![
                            ConcatPart {
                                node: Node::Dc { level: 1.0 },
                                duration_s: 0.5,
                            },
                            ConcatPart {
                                node: Node::Dc { level: 2.0 },
                                duration_s: 0.5,
                            },
                        ],
                    },
                ],
            },
        )
    }

    #[test]
    fn the_outline_lists_every_node_with_its_pointer() {
        let spec = nested();
        let outline = outline(&spec);
        let pointers: Vec<_> = outline.iter().map(|n| n.pointer.as_str()).collect();
        assert_eq!(
            pointers,
            [
                "/root",
                "/root/terms/0",
                "/root/terms/1",
                "/root/terms/1/carrier",
                "/root/terms/1/modulator",
                "/root/terms/2",
                "/root/terms/2/parts/0/node",
                "/root/terms/2/parts/1/node",
            ]
        );
        assert_eq!(outline[0].depth, 0);
        assert_eq!(outline[3].depth, 2);
        assert_eq!(outline[3].slot, "carrier");
    }

    #[test]
    fn every_pointer_the_outline_produces_resolves() {
        let spec = nested();
        for node_ref in outline(&spec) {
            let node = node_at(&spec, &node_ref.pointer)
                .unwrap_or_else(|| panic!("{} does not resolve", node_ref.pointer));
            assert_eq!(node.kind(), node_ref.kind);
        }
    }

    #[test]
    fn a_pointer_that_does_not_match_the_tree_resolves_to_nothing() {
        let spec = nested();
        for pointer in [
            "",
            "/",
            "/roots",
            "/root/terms/9",
            "/root/terms/x",
            "/root/input",
            "/root/terms/0/input",
            "/root/terms/2/parts/0",
        ] {
            assert!(node_at(&spec, pointer).is_none(), "{pointer}");
        }
    }

    #[test]
    fn replacing_a_node_leaves_its_siblings_alone() {
        let mut spec = nested();
        assert!(replace(&mut spec, "/root/terms/0", Node::Dc { level: 9.0 }));
        assert_eq!(
            node_at(&spec, "/root/terms/0"),
            Some(&Node::Dc { level: 9.0 })
        );
        assert_eq!(
            node_at(&spec, "/root/terms/1").map(Node::kind),
            Some(NodeKind::Modulate)
        );
        assert!(!replace(&mut spec, "/root/nowhere", Node::default()));
    }

    #[test]
    fn adding_and_removing_children_works_on_variable_arity_nodes_only() {
        let mut spec = nested();
        let added = add_child(&mut spec, ROOT).expect("Sum takes a child");
        assert_eq!(added, "/root/terms/3");
        assert!(node_at(&spec, &added).is_some());

        assert!(add_child(&mut spec, "/root/terms/0").is_none());

        assert_eq!(remove(&mut spec, &added).as_deref(), Some(ROOT));
        assert!(node_at(&spec, "/root/terms/3").is_none());

        // The last child of a combinator stays: an empty Sum is invalid.
        let mut small = GenSpec::new(
            1_000.0,
            1.0,
            Node::Sum {
                terms: vec![Node::default()],
            },
        );
        assert!(remove(&mut small, "/root/terms/0").is_none());
    }

    #[test]
    fn moving_a_child_reorders_it_and_reports_where_it_landed() {
        let mut spec = nested();
        let moved = move_child(&mut spec, "/root/terms/2", -1).expect("moves up");
        assert_eq!(moved, "/root/terms/1");
        assert_eq!(
            node_at(&spec, &moved).map(Node::kind),
            Some(NodeKind::Concat)
        );
        // Off either end is a no-op rather than a panic.
        assert!(move_child(&mut spec, "/root/terms/0", -1).is_none());
        assert!(move_child(&mut spec, "/root/terms/2", 1).is_none());
        // A fixed slot cannot be reordered.
        assert!(move_child(&mut spec, "/root/terms/2/carrier", -1).is_none());
    }

    #[test]
    fn a_concat_part_duration_is_reachable_from_the_concat_node() {
        let mut spec = nested();
        *part_duration_mut(&mut spec, "/root/terms/2", 1).expect("part 1") = 0.25;
        let Some(Node::Concat { parts }) = node_at(&spec, "/root/terms/2") else {
            panic!("still a concat");
        };
        assert_eq!(parts[1].duration_s, 0.25);
        assert!(part_duration_mut(&mut spec, "/root/terms/0", 0).is_none());
    }

    #[test]
    fn a_pointer_knows_its_parent() {
        assert_eq!(parent_pointer("/root"), None);
        assert_eq!(parent_pointer("/root/terms/1"), Some("/root"));
        assert_eq!(
            parent_pointer("/root/terms/1/carrier"),
            Some("/root/terms/1")
        );
        assert_eq!(
            parent_pointer("/root/terms/2/parts/0/node"),
            Some("/root/terms/2")
        );
    }

    #[test]
    fn numbers_read_without_trailing_noise() {
        assert_eq!(trim(1.0), "1");
        assert_eq!(trim(0.5), "0.5");
        assert_eq!(trim(-0.0), "0");
        assert_eq!(hz(1_000.0), "1 kHz");
        assert_eq!(hz(2_500_000.0), "2.5 MHz");
        assert_eq!(hz(50.0), "50 Hz");
    }
}
