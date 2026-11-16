//! Lexical modes for explicit interpolated strings. Embedded code is tokenized
//! once with the ordinary lexer; the parser receives those tokens with source
//! offsets, rather than parsing decoded text or synthesizing source code.
use crate::lexer::{LexError, SpannedToken, Token};
use logos::Logos;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationToken {
    pub parts: Vec<InterpolationTokenPart>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpolationTokenPart {
    Text(String),
    Expression {
        /// Byte offsets and lines relative to the containing `$"` literal.
        tokens: Vec<SpannedToken>,
        span: Range<usize>,
        end_line: usize,
        errors: Vec<(LexError, Range<usize>, usize)>,
    },
}

fn invalid_literal(quote: char, message: String) -> LexError {
    if quote == '\'' {
        LexError::InvalidChar(message)
    } else {
        LexError::InvalidString(message)
    }
}

pub(crate) fn escape(chars: &mut std::str::Chars<'_>, quote: char) -> Result<char, LexError> {
    let c = chars
        .next()
        .ok_or_else(|| invalid_literal(quote, "incomplete escape after `\\`".into()))?;
    Ok(match c {
        c if c == quote => quote,
        '\\' => '\\',
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        'x' => {
            // Commit lookahead only on success, preserving quotes and braces
            // for recovery when the escape is incomplete or malformed.
            let mut digits = chars.clone();
            let value = digits
                .next()
                .and_then(|c| c.to_digit(16))
                .zip(digits.next().and_then(|c| c.to_digit(16)))
                .map(|(high, low)| high * 16 + low)
                .ok_or_else(|| {
                    invalid_literal(
                        quote,
                        "expected two hexadecimal digits after `\\x`, for example `\\x1B`".into(),
                    )
                })?;
            if value > 0x7f {
                return Err(invalid_literal(
                    quote,
                    "hexadecimal escape is outside the ASCII range `\\x00`..`\\x7F`; use UTF-8 text for other characters".into(),
                ));
            }
            *chars = digits;
            char::from(value as u8)
        }
        _ => {
            return Err(invalid_literal(
                quote,
                format!(
                    "unknown escape `\\{}`; use `\\\\` for a literal backslash",
                    c.escape_default()
                ),
            ));
        }
    })
}

pub(crate) fn lex_interpolation(
    lex: &mut logos::Lexer<'_, Token>,
) -> Result<InterpolationToken, LexError> {
    // Bound lexer recursion independently of expression/AST recursion.
    if lex.extras.depth >= 64 {
        return Err(LexError::InvalidInterpolation(
            "string nesting exceeds 64 levels".into(),
        ));
    }
    let (result, consumed) = scan(lex.remainder(), lex.extras.nested());
    lex.bump(consumed);
    result
}

fn text(parts: &mut Vec<InterpolationTokenPart>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(InterpolationTokenPart::Text(std::mem::take(literal)));
    }
}

fn first_error(
    tokens: &[SpannedToken],
    errors: &[(LexError, Range<usize>, usize)],
) -> Option<LexError> {
    let mut errors = errors.to_vec();
    for token in tokens {
        collect_errors(&token.token, token.span.start, token.line, &mut errors);
    }
    errors.into_iter().next().map(|(error, _, _)| error)
}

fn scan(
    source: &str,
    context: crate::lexer::LexContext,
) -> (Result<InterpolationToken, LexError>, usize) {
    let mut parts = vec![];
    let mut literal = String::new();
    let mut cursor = 0;
    let mut line = 1;
    let mut invalid = None;
    while cursor < source.len() {
        let c = source[cursor..].chars().next().unwrap();
        cursor += c.len_utf8();
        match c {
            '"' => {
                text(&mut parts, &mut literal);
                return (
                    invalid.map_or_else(|| Ok(InterpolationToken { parts }), Err),
                    cursor,
                );
            }
            '\\' => {
                let mut chars = source[cursor..].chars();
                let decoded = escape(&mut chars, '"');
                let end = source.len() - chars.as_str().len();
                line += source[cursor..end].bytes().filter(|b| *b == b'\n').count();
                cursor = end;
                match decoded {
                    Ok(c) => literal.push(c),
                    Err(error) => {
                        invalid.get_or_insert(error);
                    }
                }
            }
            '{' if source[cursor..].starts_with('{') => {
                cursor += 1;
                literal.push('{');
            }
            '}' if source[cursor..].starts_with('}') => {
                cursor += 1;
                literal.push('}');
            }
            '}' => {
                invalid.get_or_insert_with(|| {
                    LexError::InvalidInterpolation(
                        "unmatched `}`; use `}}` for a literal brace".into(),
                    )
                });
            }
            '{' => {
                text(&mut parts, &mut literal);
                let start = cursor;
                let mut lexer = crate::lexer::TokenCursor::new(Token::lexer_with_extras(
                    &source[start..],
                    context.clone(),
                ));
                let mut tokens = vec![];
                let mut errors = vec![];
                let mut braces = 0usize;
                let mut parens = 0usize;
                let mut brackets = 0usize;
                let mut operand_complete = false;
                let mut scanned = 0;
                let mut token_line = line;
                let mut closed = None;
                while let Some((result, range)) = lexer.next() {
                    if !lexer.charge() {
                        return (Err(LexError::ResourceLimit("token")), start + range.end);
                    }
                    // At the top level a quote after a complete operand cannot
                    // start another valid expression token. Recover a missing
                    // `}` at the outer closing quote instead of consuming later
                    // declarations as part of an accidental nested string.
                    if operand_complete
                        && braces == 0
                        && parens == 0
                        && brackets == 0
                        && source[start + range.start..].starts_with('"')
                    {
                        return (
                            Err(first_error(&tokens, &errors).unwrap_or_else(|| {
                                LexError::InvalidInterpolation(
                                    "expected `}` before the closing quote".into(),
                                )
                            })),
                            start + range.start + 1,
                        );
                    }
                    token_line += source[start + scanned..start + range.start]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
                    scanned = range.start;
                    let token = match result {
                        Ok(Token::RBrace) if braces == 0 => {
                            closed = Some(range);
                            break;
                        }
                        Ok(token) => token,
                        Err(error) => {
                            errors.push((
                                error,
                                start + range.start + 2..start + range.end + 2,
                                token_line,
                            ));
                            Token::Invalid
                        }
                    };
                    match token {
                        Token::LBrace => braces += 1,
                        Token::RBrace => braces = braces.saturating_sub(1),
                        Token::LParen => parens += 1,
                        Token::RParen => parens = parens.saturating_sub(1),
                        Token::LBracket => brackets += 1,
                        Token::RBracket => brackets = brackets.saturating_sub(1),
                        _ => {}
                    }
                    operand_complete = match &token {
                        Token::Int(_)
                        | Token::Float(_)
                        | Token::Str(_)
                        | Token::Char(_)
                        | Token::Interpolated(_)
                        | Token::Ident(_)
                        | Token::True
                        | Token::False
                        | Token::Nil
                        | Token::RBrace
                        | Token::RParen
                        | Token::RBracket => true,
                        Token::Bang => operand_complete,
                        _ => false,
                    };
                    tokens.push(SpannedToken {
                        token,
                        span: start + range.start + 2..start + range.end + 2,
                        line: token_line,
                    });
                }
                let Some(end) = closed else {
                    let error = first_error(&tokens, &errors).unwrap_or_else(|| {
                        LexError::InvalidInterpolation("unterminated `{...}` expression".into())
                    });
                    return (Err(error), source.len());
                };
                parts.push(InterpolationTokenPart::Expression {
                    tokens,
                    span: start + 2..start + end.start + 2,
                    end_line: token_line,
                    errors,
                });
                cursor = start + end.end;
                line += source[start..cursor]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
            }
            c => {
                line += usize::from(c == '\n');
                literal.push(c);
            }
        }
    }
    let mut errors = vec![];
    collect_errors(
        &Token::Interpolated(InterpolationToken { parts }),
        0,
        1,
        &mut errors,
    );
    let error = invalid
        .or_else(|| errors.into_iter().next().map(|(error, _, _)| error))
        .unwrap_or_else(|| LexError::InvalidInterpolation("unterminated `$\"...\"` string".into()));
    (Err(error), source.len())
}

pub(crate) fn collect_errors(
    token: &Token,
    offset: usize,
    line: usize,
    output: &mut Vec<(LexError, Range<usize>, usize)>,
) {
    let Token::Interpolated(literal) = token else {
        return;
    };
    for part in &literal.parts {
        if let InterpolationTokenPart::Expression { tokens, errors, .. } = part {
            output.extend(errors.iter().map(|(error, span, relative_line)| {
                (
                    error.clone(),
                    offset + span.start..offset + span.end,
                    line + relative_line - 1,
                )
            }));
            for token in tokens {
                collect_errors(
                    &token.token,
                    offset + token.span.start,
                    line + token.line - 1,
                    output,
                );
            }
        }
    }
}
