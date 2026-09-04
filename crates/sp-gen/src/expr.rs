//! `f(t)` for [`crate::spec::Node::Expr`] (`docs/DESIGN.md` §8.1).
//!
//! The design named `meval` here. It is hand-rolled instead: `meval` 0.2 pulls
//! `nom` 1.2 (2016), which `cargo` already reports as future-incompatible, and
//! the grammar an expression node needs is a few hundred lines of
//! shunting-yard. Compiling once to RPN also keeps the per-sample cost to a
//! stack machine rather than a tree walk.
//!
//! The grammar: decimal literals (`1e-3` included), the variable `t` in
//! seconds, the constants `pi` and `e`, the operators `+ - * / % ^` with `^`
//! right-associative, unary minus, parentheses, and the functions listed in
//! [`FUNCTIONS`].

use std::fmt;

/// Every function name the grammar accepts, with its arity.
pub const FUNCTIONS: [(&str, usize); 22] = [
    ("sin", 1),
    ("cos", 1),
    ("tan", 1),
    ("asin", 1),
    ("acos", 1),
    ("atan", 1),
    ("sinh", 1),
    ("cosh", 1),
    ("tanh", 1),
    ("exp", 1),
    ("ln", 1),
    ("log10", 1),
    ("log2", 1),
    ("sqrt", 1),
    ("abs", 1),
    ("floor", 1),
    ("ceil", 1),
    ("round", 1),
    ("sign", 1),
    ("min", 2),
    ("max", 2),
    ("pow", 2),
];

/// Why an expression would not compile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ExprError {
    /// Byte offset into the source the problem was found at.
    pub at: usize,
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at character {})", self.message, self.at + 1)
    }
}

impl ExprError {
    fn new(at: usize, message: impl Into<String>) -> Self {
        Self {
            at,
            message: message.into(),
        }
    }
}

/// One instruction of the compiled stack machine.
///
/// Not `PartialEq`: two of the variants hold function pointers, whose
/// addresses carry no meaning to compare.
#[derive(Debug, Clone, Copy)]
enum Op {
    Push(f64),
    /// The variable `t`, in seconds.
    PushT,
    Neg,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Call1(fn(f64) -> f64),
    Call2(fn(f64, f64) -> f64),
}

/// A compiled expression, evaluated once per sample.
#[derive(Debug, Clone)]
pub struct Program {
    ops: Vec<Op>,
    /// High-water mark of the operand stack, so evaluation allocates nothing.
    depth: usize,
}

impl Program {
    /// Compiles `source`, or says where it stopped making sense.
    pub fn compile(source: &str) -> Result<Self, ExprError> {
        let tokens = tokenize(source)?;
        let ops = to_rpn(&tokens, source.len())?;
        let depth = stack_depth(&ops, source.len())?;
        Ok(Self { ops, depth })
    }

    /// `f(t)`. Never panics: a malformed program cannot be built.
    #[must_use]
    pub fn eval(&self, t: f64) -> f64 {
        let mut stack = Vec::with_capacity(self.depth);
        self.eval_into(t, &mut stack)
    }

    /// [`Program::eval`] reusing a caller-owned stack, for a tight render
    /// loop.
    pub(crate) fn eval_into(&self, t: f64, stack: &mut Vec<f64>) -> f64 {
        stack.clear();
        for op in &self.ops {
            match *op {
                Op::Push(value) => stack.push(value),
                Op::PushT => stack.push(t),
                Op::Neg => {
                    let a = pop(stack);
                    stack.push(-a);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
                    let b = pop(stack);
                    let a = pop(stack);
                    stack.push(match *op {
                        Op::Add => a + b,
                        Op::Sub => a - b,
                        Op::Mul => a * b,
                        Op::Div => a / b,
                        Op::Rem => a % b,
                        _ => a.powf(b),
                    });
                }
                Op::Call1(f) => {
                    let a = pop(stack);
                    stack.push(f(a));
                }
                Op::Call2(f) => {
                    let b = pop(stack);
                    let a = pop(stack);
                    stack.push(f(a, b));
                }
            }
        }
        stack.pop().unwrap_or(f64::NAN)
    }

    /// A stack sized for this program, to hand back to
    /// [`Program::eval_into`].
    pub(crate) fn stack(&self) -> Vec<f64> {
        Vec::with_capacity(self.depth)
    }
}

fn pop(stack: &mut Vec<f64>) -> f64 {
    stack.pop().unwrap_or(f64::NAN)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Op(char),
    Comma,
    Open,
    Close,
}

#[derive(Debug, Clone, PartialEq)]
struct Spanned {
    token: Token,
    at: usize,
}

fn tokenize(source: &str) -> Result<Vec<Spanned>, ExprError> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            c if c.is_ascii_whitespace() => i += 1,
            '(' => {
                out.push(Spanned {
                    token: Token::Open,
                    at: i,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    token: Token::Close,
                    at: i,
                });
                i += 1;
            }
            ',' => {
                out.push(Spanned {
                    token: Token::Comma,
                    at: i,
                });
                i += 1;
            }
            '+' | '-' | '*' | '/' | '%' | '^' => {
                out.push(Spanned {
                    token: Token::Op(c),
                    at: i,
                });
                i += 1;
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < bytes.len() && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                // An exponent, but only when a sign or digit follows, so
                // `2e` and the `e` constant stay distinguishable.
                if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                    let mut j = i + 1;
                    if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                        j += 1;
                    }
                    if j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                        while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                            j += 1;
                        }
                        i = j;
                    }
                }
                let text = &source[start..i];
                let value = text
                    .parse::<f64>()
                    .map_err(|_| ExprError::new(start, format!("'{text}' is not a number")))?;
                out.push(Spanned {
                    token: Token::Number(value),
                    at: start,
                });
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                out.push(Spanned {
                    token: Token::Ident(source[start..i].to_ascii_lowercase()),
                    at: start,
                });
            }
            other => {
                return Err(ExprError::new(i, format!("'{other}' has no meaning here")));
            }
        }
    }
    Ok(out)
}

/// Binding power and associativity of a binary operator.
const fn binary(op: char) -> (u8, bool) {
    match op {
        '+' | '-' => (1, true),
        '*' | '/' | '%' => (2, true),
        // Right-associative, so `2^3^2` is `2^9`.
        _ => (3, false),
    }
}

/// Whether `name` is a function the grammar knows. The arity in
/// [`FUNCTIONS`] is checked by [`stack_depth`] rather than here, so a call
/// with the wrong number of arguments reports one error, not two.
fn is_function(name: &str) -> bool {
    FUNCTIONS.iter().any(|(n, _)| *n == name)
}

fn call(name: &str) -> Op {
    match name {
        "sin" => Op::Call1(f64::sin),
        "cos" => Op::Call1(f64::cos),
        "tan" => Op::Call1(f64::tan),
        "asin" => Op::Call1(f64::asin),
        "acos" => Op::Call1(f64::acos),
        "atan" => Op::Call1(f64::atan),
        "sinh" => Op::Call1(f64::sinh),
        "cosh" => Op::Call1(f64::cosh),
        "tanh" => Op::Call1(f64::tanh),
        "exp" => Op::Call1(f64::exp),
        "ln" => Op::Call1(f64::ln),
        "log10" => Op::Call1(f64::log10),
        "log2" => Op::Call1(f64::log2),
        "sqrt" => Op::Call1(f64::sqrt),
        "abs" => Op::Call1(f64::abs),
        "floor" => Op::Call1(f64::floor),
        "ceil" => Op::Call1(f64::ceil),
        "round" => Op::Call1(f64::round),
        "sign" => Op::Call1(|v| if v == 0.0 { 0.0 } else { v.signum() }),
        "min" => Op::Call2(f64::min),
        "max" => Op::Call2(f64::max),
        // Only `pow` is left, and an unknown name never reaches here.
        _ => Op::Call2(f64::powf),
    }
}

/// What sits on the shunting-yard's operator stack.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Binary(char),
    Neg,
    Function(String),
    Open,
}

fn to_rpn(tokens: &[Spanned], end: usize) -> Result<Vec<Op>, ExprError> {
    if tokens.is_empty() {
        return Err(ExprError::new(0, "the expression is empty"));
    }
    let mut out: Vec<Op> = Vec::new();
    let mut stack: Vec<Pending> = Vec::new();
    // Where a value is expected, `-` is unary and `(` opens a group; where a
    // value has just been produced, `-` is subtraction.
    let mut expect_value = true;

    let emit = |out: &mut Vec<Op>, pending: &Pending| match pending {
        Pending::Binary('+') => out.push(Op::Add),
        Pending::Binary('-') => out.push(Op::Sub),
        Pending::Binary('*') => out.push(Op::Mul),
        Pending::Binary('/') => out.push(Op::Div),
        Pending::Binary('%') => out.push(Op::Rem),
        Pending::Binary(_) => out.push(Op::Pow),
        Pending::Neg => out.push(Op::Neg),
        Pending::Function(name) => out.push(call(name)),
        Pending::Open => {}
    };

    for (index, Spanned { token, at }) in tokens.iter().enumerate() {
        let at = *at;
        match token {
            Token::Number(value) => {
                if !expect_value {
                    return Err(ExprError::new(at, "an operator is missing before this"));
                }
                out.push(Op::Push(*value));
                expect_value = false;
            }
            Token::Ident(name) => {
                if !expect_value {
                    return Err(ExprError::new(at, "an operator is missing before this"));
                }
                match name.as_str() {
                    "t" => {
                        out.push(Op::PushT);
                        expect_value = false;
                    }
                    "pi" => {
                        out.push(Op::Push(std::f64::consts::PI));
                        expect_value = false;
                    }
                    "e" => {
                        out.push(Op::Push(std::f64::consts::E));
                        expect_value = false;
                    }
                    other if is_function(other) => {
                        if !matches!(
                            tokens.get(index + 1),
                            Some(Spanned {
                                token: Token::Open,
                                ..
                            })
                        ) {
                            return Err(ExprError::new(
                                at,
                                format!("'{other}' is a function and needs its arguments in ()"),
                            ));
                        }
                        stack.push(Pending::Function(other.to_owned()));
                    }
                    other => {
                        return Err(ExprError::new(
                            at,
                            format!("'{other}' is not a known name; the variable is 't'"),
                        ));
                    }
                }
            }
            Token::Op('-') if expect_value => stack.push(Pending::Neg),
            Token::Op('+') if expect_value => {}
            Token::Op(op) => {
                if expect_value {
                    return Err(ExprError::new(
                        at,
                        format!("'{op}' needs a value before it"),
                    ));
                }
                let (precedence, left) = binary(*op);
                while let Some(top) = stack.last() {
                    let higher = match top {
                        Pending::Binary(other) => {
                            let (other_precedence, _) = binary(*other);
                            other_precedence > precedence
                                || (left && other_precedence == precedence)
                        }
                        // Unary minus binds tighter than every binary
                        // operator except `^`, matching `-2^2 == -4`.
                        Pending::Neg => precedence < 3,
                        Pending::Function(..) => true,
                        Pending::Open => false,
                    };
                    if !higher {
                        break;
                    }
                    emit(&mut out, &stack.pop().expect("just inspected"));
                }
                stack.push(Pending::Binary(*op));
                expect_value = true;
            }
            Token::Comma => {
                if expect_value {
                    return Err(ExprError::new(
                        at,
                        "an argument is missing before the comma",
                    ));
                }
                loop {
                    match stack.last() {
                        Some(Pending::Open) => break,
                        Some(_) => emit(&mut out, &stack.pop().expect("just inspected")),
                        None => {
                            return Err(ExprError::new(at, "a comma outside a function call"));
                        }
                    }
                }
                expect_value = true;
            }
            Token::Open => {
                if !expect_value {
                    return Err(ExprError::new(at, "an operator is missing before this"));
                }
                stack.push(Pending::Open);
                expect_value = true;
            }
            Token::Close => {
                if expect_value {
                    return Err(ExprError::new(at, "an empty group"));
                }
                loop {
                    match stack.pop() {
                        Some(Pending::Open) => break,
                        Some(pending) => emit(&mut out, &pending),
                        None => return Err(ExprError::new(at, "an unmatched ')'")),
                    }
                }
                if let Some(Pending::Function(_)) = stack.last() {
                    emit(&mut out, &stack.pop().expect("just inspected"));
                }
                expect_value = false;
            }
        }
    }

    if expect_value {
        return Err(ExprError::new(
            end.saturating_sub(1),
            "the expression ends early",
        ));
    }
    while let Some(pending) = stack.pop() {
        if matches!(pending, Pending::Open) {
            return Err(ExprError::new(end.saturating_sub(1), "an unclosed '('"));
        }
        emit(&mut out, &pending);
    }
    if out.is_empty() {
        return Err(ExprError::new(0, "the expression is empty"));
    }
    Ok(out)
}

/// Simulates the stack so a program that would underflow is rejected at
/// compile time rather than producing NaN at render time. Returns the depth
/// the operand stack reaches.
fn stack_depth(ops: &[Op], end: usize) -> Result<usize, ExprError> {
    let mut depth: usize = 0;
    let mut peak = 0;
    for op in ops {
        let (takes, gives) = match op {
            Op::Push(_) | Op::PushT => (0, 1),
            Op::Neg | Op::Call1(_) => (1, 1),
            _ => (2, 1),
        };
        if depth < takes {
            return Err(ExprError::new(
                end.saturating_sub(1),
                "an operator is missing a value",
            ));
        }
        depth = depth - takes + gives;
        peak = peak.max(depth);
    }
    if depth != 1 {
        return Err(ExprError::new(
            end.saturating_sub(1),
            "the expression leaves more than one value",
        ));
    }
    Ok(peak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str, t: f64) -> f64 {
        Program::compile(source).expect(source).eval(t)
    }

    #[test]
    fn arithmetic_follows_precedence_and_associativity() {
        assert_eq!(eval("1 + 2 * 3", 0.0), 7.0);
        assert_eq!(eval("(1 + 2) * 3", 0.0), 9.0);
        assert_eq!(eval("2 ^ 3 ^ 2", 0.0), 512.0);
        assert_eq!(eval("-2 ^ 2", 0.0), -4.0);
        assert_eq!(eval("10 % 4", 0.0), 2.0);
        assert_eq!(eval("8 / 4 / 2", 0.0), 1.0);
    }

    #[test]
    fn the_variable_and_the_constants_resolve() {
        assert_eq!(eval("t", 2.5), 2.5);
        assert_eq!(eval("t * t", 3.0), 9.0);
        assert!((eval("pi", 0.0) - std::f64::consts::PI).abs() < f64::EPSILON);
        assert!((eval("e", 0.0) - std::f64::consts::E).abs() < f64::EPSILON);
    }

    #[test]
    fn functions_take_their_arity() {
        assert_eq!(eval("max(2, 5)", 0.0), 5.0);
        assert_eq!(eval("min(2, 5)", 0.0), 2.0);
        assert_eq!(eval("pow(2, 10)", 0.0), 1024.0);
        assert_eq!(eval("abs(-3)", 0.0), 3.0);
        assert_eq!(eval("sign(-0.5)", 0.0), -1.0);
        assert_eq!(eval("sign(0)", 0.0), 0.0);
        assert!(eval("sin(pi/2)", 0.0) > 0.999_999);
    }

    #[test]
    fn scientific_notation_parses_and_e_still_reads_as_a_constant() {
        assert_eq!(eval("1e-3", 0.0), 0.001);
        assert_eq!(eval("2E3", 0.0), 2000.0);
        // `2*e` is two times Euler's number, not a malformed literal.
        assert!((eval("2*e", 0.0) - 2.0 * std::f64::consts::E).abs() < 1e-12);
    }

    #[test]
    fn unary_signs_stack() {
        assert_eq!(eval("--3", 0.0), 3.0);
        assert_eq!(eval("+-3", 0.0), -3.0);
        assert_eq!(eval("3 - -3", 0.0), 6.0);
    }

    #[test]
    fn a_realistic_waveform_expression_compiles() {
        let program = Program::compile("sin(2*pi*1000*t) * exp(-t/0.01)").unwrap();
        assert_eq!(program.eval(0.0), 0.0);
        assert!(program.eval(0.00025).abs() > 0.9);
    }

    #[test]
    fn broken_expressions_say_where_and_why() {
        for (source, fragment) in [
            ("1 +", "ends early"),
            ("(1 + 2", "unclosed"),
            ("1 + 2)", "unmatched"),
            ("2 2", "operator is missing"),
            ("wobble(t)", "not a known name"),
            ("* 2", "needs a value"),
            ("", "empty"),
            ("1 $ 2", "no meaning"),
            ("()", "empty group"),
            ("max(,2)", "argument is missing"),
            ("sin + 1", "needs its arguments"),
        ] {
            let error = Program::compile(source).expect_err(source);
            assert!(
                error.message.contains(fragment),
                "{source}: {error} does not mention {fragment}"
            );
        }
    }

    #[test]
    fn a_wrong_argument_count_is_caught_at_compile_time() {
        // `max` takes two; one leaves the stack short.
        assert!(Program::compile("max(1)").is_err());
        // Three leaves a value behind.
        assert!(Program::compile("max(1, 2, 3)").is_err());
    }

    #[test]
    fn evaluation_is_allocation_free_once_a_stack_exists() {
        let program = Program::compile("sin(t) + cos(t) * 2").unwrap();
        let mut stack = program.stack();
        let capacity = stack.capacity();
        for i in 0..100 {
            program.eval_into(f64::from(i), &mut stack);
        }
        assert_eq!(stack.capacity(), capacity);
    }
}
