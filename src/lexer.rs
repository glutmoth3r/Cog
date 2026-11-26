//! Cog tokenization, including source spans and the shared budget for nested
//! interpolation. The lexer reports invalid tokens without deciding whether the
//! resulting syntax is semantically valid; [`crate::hlparser`] handles that phase.
pub use crate::string_literals::{InterpolationToken, InterpolationTokenPart};
use logos::Logos;
use num_bigint::BigInt;

#[derive(Debug, Clone)]
pub struct LexContext {
    pub(crate) depth: usize,
    remaining: std::rc::Rc<std::cell::Cell<usize>>,
}
impl Default for LexContext {
    fn default() -> Self {
        Self::with_tokens(crate::limits::CompilerLimits::default().max_tokens)
    }
}
impl LexContext {
    fn with_tokens(tokens: usize) -> Self {
        Self {
            depth: 0,
            remaining: std::rc::Rc::new(std::cell::Cell::new(tokens)),
        }
    }
    pub(crate) fn nested(&self) -> Self {
        Self {
            depth: self.depth + 1,
            remaining: self.remaining.clone(),
        }
    }
    pub(crate) fn charge(&self) -> bool {
        let Some(remaining) = self.remaining.get().checked_sub(1) else {
            return false;
        };
        self.remaining.set(remaining);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum LexError {
    #[default]
    Other,
    ResourceLimit(&'static str),
    InvalidInt(String),
    InvalidFloat(String),
    InvalidString(String),
    InvalidChar(String),
    InvalidInterpolation(String),
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LexError::ResourceLimit(resource) => write!(f, "compiler {resource} limit exceeded"),
            LexError::Other => write!(f, "invalid token"),
            LexError::InvalidInt(s) => write!(f, "invalid integer literal `{s}`"),
            LexError::InvalidFloat(s) => write!(f, "invalid float literal `{s}`"),
            LexError::InvalidString(message) => write!(f, "invalid string literal: {message}"),
            LexError::InvalidChar(message) => write!(f, "invalid character literal: {message}"),
            LexError::InvalidInterpolation(message) => {
                write!(f, "invalid interpolation: {message}")
            }
        }
    }
}

fn unescape_string(lex: &mut logos::Lexer<Token>) -> Result<String, LexError> {
    let raw = lex.slice();
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escaped = crate::string_literals::escape(&mut chars, '"')?;
        out.push(escaped);
    }
    Ok(out)
}

fn parse_char(lex: &mut logos::Lexer<Token>) -> Result<char, LexError> {
    let raw = lex.slice();
    let inner = &raw[1..raw.len() - 1];
    let mut chars = inner.chars();
    let Some(first) = chars.next() else {
        return Err(LexError::InvalidChar(
            "expected exactly one Unicode scalar value".into(),
        ));
    };
    let value = if first == '\\' {
        crate::string_literals::escape(&mut chars, '\'')?
    } else {
        first
    };
    if chars.next().is_some() {
        return Err(LexError::InvalidChar(
            "expected exactly one Unicode scalar value".into(),
        ));
    }
    Ok(value)
}

fn parse_int(lex: &mut logos::Lexer<Token>) -> Result<u64, LexError> {
    integer_value(lex.slice())
}

fn integer_value(raw: &str) -> Result<u64, LexError> {
    let normalized = raw.replace('_', "");
    let parsed = if let Some(hex) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        normalized.parse::<u64>()
    };
    parsed.map_err(|_| LexError::InvalidInt(raw.to_owned()))
}

fn parse_bigint(lex: &mut logos::Lexer<Token>) -> Result<BigInt, LexError> {
    let raw = lex.slice();
    let normalized = raw.replace('_', "");
    let (digits, radix) = if let Some(hex) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        (hex, 16)
    } else {
        (normalized.as_str(), 10)
    };
    BigInt::parse_bytes(digits.as_bytes(), radix)
        .ok_or_else(|| LexError::InvalidInt(raw.to_owned()))
}

fn parse_float(lex: &mut logos::Lexer<Token>) -> Result<f64, LexError> {
    let raw = lex.slice();
    raw.replace('_', "")
        .parse::<f64>()
        .map_err(|_| LexError::InvalidFloat(raw.to_owned()))
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(error = LexError)]
#[logos(extras = LexContext)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
#[logos(skip r"/\*([^*]|\*[^/])*\*/")]
pub enum Token {
    /// Inserted by recovery, never produced by a valid token rule.
    Invalid,
    #[regex(r"[0-9](?:_?[0-9])*(?:\.[0-9](?:_?[0-9])*(?:[eE][+-]?[0-9](?:_?[0-9])*)?|[eE][+-]?[0-9](?:_?[0-9])*)", parse_float)]
    Float(f64),

    #[allow(unknown_lints, clippy::duplicated_attributes)]
    #[regex(r"0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*", parse_int)]
    #[regex(r"[0-9](?:_?[0-9])*", parse_int)]
    Int(u64),

    #[allow(unknown_lints, clippy::duplicated_attributes)]
    #[regex(r"0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f]){16,}", parse_bigint)]
    #[regex(r"[0-9](?:_?[0-9]){19,}", parse_bigint)]
    BigInt(BigInt),

    #[regex(r#""([^"\\]|\\.)*""#, unescape_string)]
    Str(String),

    #[regex(r#"'([^'\\]|\\.)*'"#, parse_char)]
    Char(char),

    #[token("$\"", crate::string_literals::lex_interpolation)]
    Interpolated(InterpolationToken),

    #[token("true")]
    True,
    #[token("false")]
    False,

    #[token("nil")]
    Nil,

    #[token("fn")]
    Fn,

    #[token("public")]
    Public,

    #[token("private")]
    Private,

    #[token("struct")]
    Struct,

    #[token("enum")]
    Enum,

    #[token("use")]
    Use,

    #[token("var")]
    Var,

    #[token("const")]
    Const,

    #[token("if")]
    If,

    #[token("else")]
    Else,

    #[token("while")]
    While,

    #[token("for")]
    For,

    #[token("break")]
    Break,

    #[token("continue")]
    Continue,

    #[token("return")]
    Return,

    #[token("defer")]
    Defer,

    #[token("try")]
    Try,

    #[token("catch")]
    Catch,

    #[token("crash")]
    Crash,

    #[regex(r"[A-Za-z_][A-Za-z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),

    #[regex(r"(?:\.\.?/)+[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*", |lex| lex.slice().to_string())]
    Path(String),

    #[token("+")]
    Plus, //python behaviour
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,

    #[token("<<")]
    Shl,

    #[token(">>")]
    Shr,

    #[token("==")]
    EqEq,

    #[token("!=")]
    NotEq,

    #[token("<=")]
    LtEq,

    #[token(">=")]
    GtEq,

    #[token("<")]
    Lt,

    #[token(">")]
    Gt,

    #[token("&&")]
    AmpAmp,

    #[token("||")]
    PipePipe,

    #[token("^")]
    Caret, //xor

    #[token("!")]
    Bang,

    #[token("@")]
    At,

    #[token("??")]
    NilFallback,
    #[token("?!")]
    ErrorFallback,

    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("%=")]
    PercentEq,
    #[token("&&=")]
    AmpAmpEq,
    #[token("||=")]
    PipePipeEq,
    #[token("^=")]
    CaretEq,
    #[token("<<=")]
    ShlEq,
    #[token(">>=")]
    ShrEq,

    #[token("=")]
    Eq,

    #[token(".")]
    Dot,

    #[token("..")]
    DotDot,

    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    #[token("{")]
    LBrace,

    #[token("}")]
    RBrace,

    #[token("[")]
    LBracket,

    #[token("]")]
    RBracket,

    #[token(",")]
    Comma,

    #[token(";")]
    Semi,

    #[token(":")]
    Colon,

    #[token("::")]
    DoubleColon,

    #[token("=>")]
    MatchArrow,

    #[token("->")]
    Arrow,

    #[token("when")]
    When,

    #[token("is")]
    Is,

    // `expr as Type` (cast) and `use mod::name as alias;` (import alias) —
    // same token, two grammar positions. See hlparser::parse_cast and
    // hlparser::parse_use_item.
    #[token("as")]
    As,

    #[token("|")]
    Pipe,

    #[token("match")]
    Match,

    #[token("?")]
    Question,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub line: usize,
    pub span: std::ops::Range<usize>,
}

pub struct LexOutput {
    pub tokens: Vec<SpannedToken>,
    pub errors: Vec<(LexError, std::ops::Range<usize>, usize)>,
}

/// Resolve adjacent numeric fields before parsing, including embedded code.
/// Logos maximally munches `pair.0.1` as Ident, Dot, Float; after a field dot
/// the decimal components must instead retain their individual integer spans.
pub(crate) struct TokenCursor<'source> {
    lexer: logos::Lexer<'source, Token>,
    pending: Option<std::ops::Range<usize>>,
    after_dot: bool,
}

impl<'source> TokenCursor<'source> {
    pub(crate) fn new(lexer: logos::Lexer<'source, Token>) -> Self {
        Self {
            lexer,
            pending: None,
            after_dot: false,
        }
    }

    pub(crate) fn charge(&self) -> bool {
        self.lexer.extras.charge()
    }
}

impl Iterator for TokenCursor<'_> {
    type Item = (Result<Token, LexError>, std::ops::Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        let (result, span) = if let Some(span) = self.pending.take() {
            if self.lexer.source().as_bytes()[span.start] == b'.' {
                self.pending = Some(span.start + 1..span.end);
                (Ok(Token::Dot), span.start..span.start + 1)
            } else {
                (
                    integer_value(&self.lexer.source()[span.clone()]).map(Token::Int),
                    span,
                )
            }
        } else {
            let result = self.lexer.next()?;
            let mut span = self.lexer.span();
            let raw = self.lexer.slice();
            let result = if self.after_dot
                && matches!(result, Ok(Token::Float(_)) | Err(LexError::InvalidFloat(_)))
                && !raw.contains(['e', 'E'])
                && let Some(dot) = raw.find('.')
            {
                self.pending = Some(span.start + dot..span.end);
                span.end = span.start + dot;
                integer_value(&raw[..dot]).map(Token::Int)
            } else {
                result
            };
            (result, span)
        };
        self.after_dot = matches!(result, Ok(Token::Dot));
        Some((result, span))
    }
}

pub fn tokenize_recovering(source: &str) -> LexOutput {
    tokenize_with_limits(source, crate::limits::CompilerLimits::default())
}

pub fn tokenize_with_limits(source: &str, limits: crate::limits::CompilerLimits) -> LexOutput {
    let limits = limits.normalized();
    if source.len() > limits.max_source_bytes {
        return LexOutput {
            tokens: Vec::new(),
            errors: vec![(LexError::ResourceLimit("source byte"), 0..0, 1)],
        };
    }
    let mut lex = Token::lexer_with_extras(source, LexContext::with_tokens(limits.max_tokens));
    // Keep the original source/offsets for diagnostics. A shebang is accepted
    // only at byte zero; the following newline is counted by the normal scan.
    if source.starts_with("#!") {
        lex.bump(source.find('\n').unwrap_or(source.len()));
    }
    let mut lex = TokenCursor::new(lex);
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut scanned = 0;
    let mut line = 1;
    while let Some((result, span)) = lex.next() {
        if !lex.charge() {
            errors.push((LexError::ResourceLimit("token"), span, line));
            break;
        }
        line += source[scanned..span.start]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        scanned = span.start;
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                let stop = matches!(error, LexError::ResourceLimit(_));
                errors.push((error, span.clone(), line));
                if stop || errors.len() >= limits.max_diagnostics {
                    break;
                }
                Token::Invalid
            }
        };
        crate::string_literals::collect_errors(&token, span.start, line, &mut errors);
        tokens.push(SpannedToken { token, line, span });
        if errors.len() >= limits.max_diagnostics {
            errors.truncate(limits.max_diagnostics);
            break;
        }
    }
    LexOutput { tokens, errors }
}

pub fn tokenize(source: &str) -> Result<Vec<SpannedToken>, (LexError, usize)> {
    let output = tokenize_recovering(source);
    match output.errors.into_iter().next() {
        Some((error, _, line)) => Err((error, line)),
        None => Ok(output.tokens),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shebang_preserves_source_offsets_and_only_skips_the_first_line() {
        for header in [
            "#!/usr/bin/env cog\n",
            "#!/usr/bin/env -S cog run\r\n",
            "#! Cog 🦀\n",
        ] {
            let source = format!("{header}fn main() {{}}\n#!invalid");
            let output = super::tokenize_recovering(&source);
            assert_eq!(output.tokens[0].token, super::Token::Fn);
            assert_eq!(output.tokens[0].span, header.len()..header.len() + 2);
            assert_eq!(output.tokens[0].line, 2);
            assert_eq!(output.errors[0].2, 3);
        }
        assert!(super::tokenize("#!/usr/bin/env cog").unwrap().is_empty());
        assert!(super::tokenize(" #!/usr/bin/env cog").is_err());
    }
    use super::*;

    #[test]
    fn basic_function() {
        let src = "fn add(a, b) {\n    return a + b;\n}\n";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Fn,
                Token::Ident("add".to_string()),
                Token::LParen,
                Token::Ident("a".to_string()),
                Token::Comma,
                Token::Ident("b".to_string()),
                Token::RParen,
                Token::LBrace,
                Token::Return,
                Token::Ident("a".to_string()),
                Token::Plus,
                Token::Ident("b".to_string()),
                Token::Semi,
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn numeric_literals_accept_separators_hex_and_decimal_exponents() {
        let tokens = tokenize("1_000 0xFF 0XCA_FE 1_2.3_4e+5_6 7E-2")
            .expect("valid numeric literals should tokenize");
        assert_eq!(tokens[0].token, Token::Int(1000));
        assert_eq!(tokens[1].token, Token::Int(255));
        assert_eq!(tokens[2].token, Token::Int(0xCAFE));
        assert!(matches!(tokens[3].token, Token::Float(value) if (value - 1.234e57).abs() < 1e42));
        assert!(matches!(tokens[4].token, Token::Float(value) if (value - 0.07).abs() < 1e-12));
    }

    #[test]
    fn numeric_tuple_fields_keep_individual_tokens_and_source_spans() {
        let source = "// header\npair.01.0_2.3.4 1.0 7E-2 0..2 1.0..2.0 pair.9007199254740993.9223372036854775807";
        let tokens = tokenize(source).unwrap();
        let expected = [
            (Token::Ident("pair".into()), "pair"),
            (Token::Dot, "."),
            (Token::Int(1), "01"),
            (Token::Dot, "."),
            (Token::Int(2), "0_2"),
            (Token::Dot, "."),
            (Token::Int(3), "3"),
            (Token::Dot, "."),
            (Token::Int(4), "4"),
            (Token::Float(1.0), "1.0"),
            (Token::Float(0.07), "7E-2"),
            (Token::Int(0), "0"),
            (Token::DotDot, ".."),
            (Token::Int(2), "2"),
            (Token::Float(1.0), "1.0"),
            (Token::DotDot, ".."),
            (Token::Float(2.0), "2.0"),
            (Token::Ident("pair".into()), "pair"),
            (Token::Dot, "."),
            (Token::Int(9007199254740993), "9007199254740993"),
            (Token::Dot, "."),
            (Token::Int(i64::MAX as u64), "9223372036854775807"),
        ];
        assert_eq!(tokens.len(), expected.len());
        for (token, (kind, raw)) in tokens.iter().zip(expected) {
            assert_eq!(token.token, kind);
            assert_eq!(&source[token.span.clone()], raw);
            assert_eq!(token.line, 2);
        }
        let tokens = tokenize("pair. /* field */\n 0.1").unwrap();
        assert_eq!(tokens[2].token, Token::Int(0));
        assert_eq!(tokens[2].line, 2);
        assert_eq!(tokens[4].token, Token::Int(1));
        assert_eq!(tokens[4].line, 2);
    }

    #[test]
    fn numeric_tuple_fields_preserve_interpolation_offsets() {
        let source = "// header\n$\"{pair.0.1}\"";
        let outer = tokenize(source).unwrap().remove(0);
        let Token::Interpolated(literal) = outer.token else {
            panic!("expected an interpolated string");
        };
        let InterpolationTokenPart::Expression { tokens, errors, .. } = &literal.parts[0] else {
            panic!("expected an interpolation expression");
        };
        assert!(errors.is_empty());
        let expected = [
            (Token::Ident("pair".into()), "pair"),
            (Token::Dot, "."),
            (Token::Int(0), "0"),
            (Token::Dot, "."),
            (Token::Int(1), "1"),
        ];
        assert_eq!(tokens.len(), expected.len());
        for (token, (kind, raw)) in tokens.iter().zip(expected) {
            assert_eq!(token.token, kind);
            let span = outer.span.start + token.span.start..outer.span.start + token.span.end;
            assert_eq!(&source[span], raw);
            assert_eq!(outer.line + token.line - 1, 2);
        }
    }

    #[test]
    fn numeric_tuple_fields_charge_each_token_including_interpolation() {
        for (source, count) in [
            ("pair.0.1", 5),
            ("$\"{pair.0.1}\"", 7),
            ("$\"{$\"{pair.0.1}\"}\"", 9),
        ] {
            for budget in 0..=count {
                let output = tokenize_with_limits(
                    source,
                    crate::limits::CompilerLimits {
                        max_tokens: budget,
                        ..crate::limits::CompilerLimits::default()
                    },
                );
                if budget == count {
                    assert!(output.errors.is_empty(), "{source}: {:?}", output.errors);
                } else {
                    assert!(
                        output.errors.iter().any(|(error, _, _)| {
                            matches!(error, LexError::ResourceLimit("token"))
                        }),
                        "{source} with budget {budget}: {:?}",
                        output.errors
                    );
                }
            }
        }
    }

    #[test]
    fn numeric_tuple_field_above_signed_range_preserves_uint_and_span() {
        let field = 9_223_372_036_854_775_808u64;
        for source in [
            "// header\npair.9223372036854775808.0; after",
            "// header\npair.0.9223372036854775808; after",
            "// header\n$\"{pair.0.9223372036854775808}\"; after",
        ] {
            let output = tokenize_recovering(source);
            assert!(output.errors.is_empty(), "{:?}", output.errors);
            let start = source.find("9223372036854775808").unwrap();
            let end = start + "9223372036854775808".len();
            let direct = output.tokens.iter().any(|token| {
                token.token == Token::Int(field) && token.span == (start..end) && token.line == 2
            });
            let interpolated = output.tokens.iter().any(|outer| {
                let Token::Interpolated(literal) = &outer.token else {
                    return false;
                };
                literal.parts.iter().any(|part| {
                    let InterpolationTokenPart::Expression { tokens, .. } = part else {
                        return false;
                    };
                    tokens.iter().any(|token| {
                        let span =
                            outer.span.start + token.span.start..outer.span.start + token.span.end;
                        token.token == Token::Int(field) && span == (start..end)
                    })
                })
            });
            assert!(direct || interpolated, "{source}: {:?}", output.tokens);
            assert_eq!(
                output.tokens.last().unwrap().token,
                Token::Ident("after".into())
            );
        }
    }

    #[test]
    fn hexadecimal_integer_above_signed_range_is_a_uint_token() {
        let tokens = tokenize("0x8000000000000000").unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token, Token::Int(1u64 << 63));
    }

    #[test]
    fn shift_operators_and_compound_assignments() {
        let src = "a << 3; b >> 1; a <<= 2; b >>= c; x >= y;";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        let ident = |name: &str| Token::Ident(name.to_string());
        assert_eq!(
            kinds,
            vec![
                ident("a"),
                Token::Shl,
                Token::Int(3),
                Token::Semi,
                ident("b"),
                Token::Shr,
                Token::Int(1),
                Token::Semi,
                ident("a"),
                Token::ShlEq,
                Token::Int(2),
                Token::Semi,
                ident("b"),
                Token::ShrEq,
                ident("c"),
                Token::Semi,
                ident("x"),
                Token::GtEq,
                ident("y"),
                Token::Semi,
            ]
        );
    }

    #[test]
    fn keywords_beat_identifier_regex() {
        let tokens =
            tokenize("fn var if else while for return true false crash").expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Fn,
                Token::Var,
                Token::If,
                Token::Else,
                Token::While,
                Token::For,
                Token::Return,
                Token::True,
                Token::False,
                Token::Crash,
            ]
        );
    }

    #[test]
    fn package_path_tokens_preserve_parent_components() {
        let tokens = tokenize("../../game").expect("relative package path should tokenize");
        assert_eq!(
            tokens,
            vec![SpannedToken {
                token: Token::Path("../../game".into()),
                line: 1,
                span: 0..10
            }]
        );
    }

    #[test]
    fn string_escapes() {
        let tokens = tokenize(r#""hi\n\"there\"""#).expect("should tokenize");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token, Token::Str("hi\n\"there\"".to_string()));
    }

    #[test]
    fn string_escapes_are_single_pass_and_unknown_escapes_are_errors() {
        let tokens = tokenize(r#""\\n\r\0\t\\\"""#).unwrap();
        assert_eq!(tokens[0].token, Token::Str("\\n\r\0\t\\\"".into()));
        assert!(matches!(
            tokenize(r#""bad\q""#),
            Err((LexError::InvalidString(_), 1))
        ));
    }

    #[test]
    fn line_tracking() {
        let src = "let a = 1;\nlet b = 2;\n";
        let tokens = tokenize(src).expect("should tokenize");
        // `b` is the 6th token, on line 2.
        let b_tok = tokens
            .iter()
            .find(|t| t.token == Token::Ident("b".to_string()))
            .unwrap();
        assert_eq!(b_tok.line, 2);
    }

    #[test]
    fn comments_are_skipped() {
        let src = "var a = 1; // trailing comment\n/* block\ncomment */ var b = 2;";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Var,
                Token::Ident("a".to_string()),
                Token::Eq,
                Token::Int(1),
                Token::Semi,
                Token::Var,
                Token::Ident("b".to_string()),
                Token::Eq,
                Token::Int(2),
                Token::Semi,
            ]
        );
    }
}
