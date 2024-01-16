use std::fmt::Display;

use bitflags::bitflags;

use ast::{
    BoolOp, CmpOp, ConversionFlag, ExceptHandler, ExprContext, FStringElement, IpyEscapeKind, Mod,
    Number, Operator, Pattern, Singleton, UnaryOp,
};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::lexer::lex;
use crate::{
    error::FStringErrorType,
    lexer::{LexResult, Spanned},
    string::{
        concatenated_strings, parse_fstring_literal_element, parse_string_literal, StringType,
    },
    token_set::TokenSet,
    token_source::TokenSource,
    Mode, ParseError, ParseErrorType, Tok, TokenKind,
};

use self::helpers::token_kind_to_cmp_op;

mod helpers;
#[cfg(test)]
mod tests;

pub(crate) fn parse_tokens(
    tokens: Vec<LexResult>,
    source: &str,
    mode: Mode,
) -> Result<Mod, ParseError> {
    let program = Parser::new(source, mode, TokenSource::new(tokens)).parse();
    if program.parse_errors.is_empty() {
        Ok(program.ast)
    } else {
        Err(program.parse_errors.into_iter().next().unwrap())
    }
}

#[derive(Debug)]
pub struct Program {
    pub ast: ast::Mod,
    pub parse_errors: Vec<ParseError>,
}

impl Program {
    pub fn parse_str(source: &str, mode: Mode) -> Program {
        let tokens = lex(source, mode);
        Self::parse_tokens(source, tokens.collect(), mode)
    }

    pub fn parse_tokens(source: &str, tokens: Vec<LexResult>, mode: Mode) -> Program {
        Parser::new(source, mode, TokenSource::new(tokens)).parse()
    }
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    struct ParserCtxFlags: u8 {
        const PARENTHESIZED_EXPR = 1 << 0;

        // NOTE: `ARGUMENTS` can be removed once the heuristic in `parse_with_items`
        // is improved.
        const ARGUMENTS = 1 << 1;
        const FOR_TARGET = 1 << 2;
    }
}

type ExprWithRange = (ParsedExpr, TextRange);

#[derive(Debug)]
struct ParsedExpr {
    expr: Expr,
    is_parenthesized: bool,
}

impl From<Expr> for ParsedExpr {
    fn from(expr: Expr) -> Self {
        ParsedExpr {
            expr,
            is_parenthesized: false,
        }
    }
}

/// Binding power associativity
enum Associativity {
    Left,
    Right,
}

#[derive(Copy, Clone)]
enum Clause {
    If,
    Else,
    ElIf,
    For,
    With,
    Class,
    While,
    FunctionDef,
    Match,
    Try,
    Except,
    Finally,
}

impl Display for Clause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Clause::If => write!(f, "`if` statement"),
            Clause::Else => write!(f, "`else` clause"),
            Clause::ElIf => write!(f, "`elif` clause"),
            Clause::For => write!(f, "`for` statement"),
            Clause::With => write!(f, "`with` statement"),
            Clause::Class => write!(f, "`class` definition"),
            Clause::While => write!(f, "`while` statement"),
            Clause::FunctionDef => write!(f, "function definition"),
            Clause::Match => write!(f, "`match` statement"),
            Clause::Try => write!(f, "`try` statement"),
            Clause::Except => write!(f, "`except` clause"),
            Clause::Finally => write!(f, "`finally` clause"),
        }
    }
}

#[derive(PartialEq, Copy, Clone)]
enum FunctionKind {
    Lambda,
    FunctionDef,
}

pub(crate) struct Parser<'src> {
    source: &'src str,
    tokens: TokenSource,

    /// Stores all the syntax errors found during the parsing.
    errors: Vec<ParseError>,

    /// This tracks the current expression or statement being parsed. For example,
    /// if we're parsing a tuple expression, e.g. `(1, 2)`, `ctx` has the value
    /// `ParserCtxFlags::TUPLE_EXPR`.
    ///
    /// The `ctx` is also used to create custom error messages and forbid certain
    /// expressions or statements of being parsed. The `ctx` should be empty after
    /// an expression or statement is done parsing.
    ctx: ParserCtxFlags,

    /// During the parsing of expression or statement, multiple `ctx`s can be created.
    /// `ctx_stack` stores the previous `ctx`s that were created during the parsing. For example,
    /// when parsing a tuple expression, e.g. `(1, 2, 3)`, two [`ParserCtxFlags`] will be
    /// created `ParserCtxFlags::PARENTHESIZED_EXPR` and `ParserCtxFlags::TUPLE_EXPR`.
    ///
    /// When parsing a tuple the first context created is `ParserCtxFlags::PARENTHESIZED_EXPR`.
    /// Afterwards, the `ParserCtxFlags::TUPLE_EXPR` is created and `ParserCtxFlags::PARENTHESIZED_EXPR`
    /// is pushed onto the `ctx_stack`.
    /// `ParserCtxFlags::PARENTHESIZED_EXPR` is removed from the stack and set to be the current `ctx`,
    /// after we parsed all elements in the tuple.
    ///
    /// The end of the vector is the top of the stack.
    ctx_stack: Vec<ParserCtxFlags>,

    /// Stores the last `ctx` of an expression or statement that was parsed.
    last_ctx: ParserCtxFlags,

    /// Specify the mode in which the code will be parsed.
    mode: Mode,

    /// Defer the creation of the invalid node for the skipped unexpected tokens.
    /// Holds the range of the skipped tokens.
    defer_invalid_node_creation: Option<TextRange>,

    current: Spanned,

    /// The end of the last processed. Used to determine a node's end.
    last_token_end: TextSize,
}

const NEWLINE_EOF_SET: TokenSet = TokenSet::new(&[TokenKind::Newline, TokenKind::EndOfFile]);
const LITERAL_SET: TokenSet = TokenSet::new(&[
    TokenKind::Name,
    TokenKind::Int,
    TokenKind::Float,
    TokenKind::Complex,
    TokenKind::Plus,
    TokenKind::String,
    TokenKind::Ellipsis,
    TokenKind::True,
    TokenKind::False,
    TokenKind::None,
]);
/// Tokens that are usually an expression or the start of one.
const EXPR_SET: TokenSet = TokenSet::new(&[
    TokenKind::Minus,
    TokenKind::Tilde,
    TokenKind::Star,
    TokenKind::DoubleStar,
    TokenKind::Vbar,
    TokenKind::Lpar,
    TokenKind::Lbrace,
    TokenKind::Lsqb,
    TokenKind::Lambda,
    TokenKind::Await,
    TokenKind::Not,
    TokenKind::Yield,
    TokenKind::FStringStart,
])
.union(LITERAL_SET);
/// Tokens that can appear after an expression.
const END_EXPR_SET: TokenSet = TokenSet::new(&[
    TokenKind::Newline,
    TokenKind::Semi,
    TokenKind::Colon,
    TokenKind::EndOfFile,
    TokenKind::Rbrace,
    TokenKind::Rsqb,
    TokenKind::Rpar,
    TokenKind::Comma,
    TokenKind::Dedent,
    TokenKind::Else,
    TokenKind::As,
    TokenKind::From,
    TokenKind::For,
    TokenKind::Async,
    TokenKind::In,
]);
/// Tokens that represent compound statements.
const COMPOUND_STMT_SET: TokenSet = TokenSet::new(&[
    TokenKind::Match,
    TokenKind::If,
    TokenKind::Else,
    TokenKind::Elif,
    TokenKind::With,
    TokenKind::While,
    TokenKind::For,
    TokenKind::Try,
    TokenKind::Def,
    TokenKind::Class,
    TokenKind::Async,
]);
/// Tokens that represent simple statements, but doesn't include expressions.
const SIMPLE_STMT_SET: TokenSet = TokenSet::new(&[
    TokenKind::Pass,
    TokenKind::Return,
    TokenKind::Break,
    TokenKind::Continue,
    TokenKind::Global,
    TokenKind::Nonlocal,
    TokenKind::Assert,
    TokenKind::Yield,
    TokenKind::Del,
    TokenKind::Raise,
    TokenKind::Import,
    TokenKind::From,
    TokenKind::Type,
]);
/// Tokens that represent simple statements, including expressions.
const SIMPLE_STMT_SET2: TokenSet = SIMPLE_STMT_SET.union(EXPR_SET);

impl<'src> Parser<'src> {
    pub(crate) fn new(source: &'src str, mode: Mode, mut tokens: TokenSource) -> Parser<'src> {
        let current = tokens
            .next()
            .unwrap_or_else(|| (Tok::EndOfFile, TextRange::empty(source.text_len())));

        Parser {
            mode,
            source,
            errors: Vec::new(),
            ctx_stack: Vec::new(),
            ctx: ParserCtxFlags::empty(),
            last_ctx: ParserCtxFlags::empty(),
            tokens,
            current,

            last_token_end: TextSize::default(),
            defer_invalid_node_creation: None,
        }
    }
    fn finish(self) -> Vec<ParseError> {
        // After parsing, the `ctx` and `ctx_stack` should be empty.
        // If it's not, you probably forgot to call `clear_ctx` somewhere.
        assert_eq!(self.ctx, ParserCtxFlags::empty());
        assert_eq!(&self.ctx_stack, &[]);
        assert_eq!(
            self.current,
            (Tok::EndOfFile, TextRange::empty(self.source.text_len())),
            "Parser should be at the end of the file."
        );

        // TODO consider re-integrating lexical error handling into the aprser?
        let parse_errors = self.errors;
        let lex_errors = self.tokens.finish();

        // Fast path for when there are no lex errors.
        // There's no fast path for when there are no parse errors because a lex error
        // always results in a parse error.
        if lex_errors.is_empty() {
            return parse_errors;
        }

        let mut merged = Vec::with_capacity(parse_errors.len().saturating_add(lex_errors.len()));

        let mut parse_errors = parse_errors.into_iter().peekable();
        let mut lex_errors = lex_errors.into_iter().peekable();

        while let (Some(parse_error), Some(lex_error)) = (parse_errors.peek(), lex_errors.peek()) {
            if parse_error.location.start() < lex_error.location.start() {
                merged.push(parse_errors.next().unwrap());
            } else {
                merged.push(ParseError::from(lex_errors.next().unwrap()));
            }
        }

        merged.extend(parse_errors);
        merged.extend(lex_errors.map(ParseError::from));

        merged
    }

    pub(crate) fn parse(mut self) -> Program {
        let mut body = vec![];

        let ast = if self.mode == Mode::Expression {
            let (parsed_expr, range) = self.parse_exprs();
            loop {
                if !self.eat(TokenKind::Newline) {
                    break;
                }
            }
            self.expect(TokenKind::EndOfFile);

            ast::Mod::Expression(ast::ModExpression {
                body: Box::new(parsed_expr.expr),
                range,
            })
        } else {
            let is_src_empty = self.at(TokenKind::EndOfFile);
            while !self.at(TokenKind::EndOfFile) {
                if self.at(TokenKind::Indent) {
                    self.handle_unexpected_indentation(&mut body, "unexpected indentation");
                    continue;
                }

                body.push(self.parse_statement());

                if let Some(range) = self.defer_invalid_node_creation {
                    self.defer_invalid_node_creation = None;
                    body.push(Stmt::Expr(ast::StmtExpr {
                        value: Box::new(Expr::Invalid(ast::ExprInvalid {
                            value: self.src_text(range).into(),
                            range,
                        })),
                        range,
                    }));
                }
            }
            ast::Mod::Module(ast::ModModule {
                body,
                // If the `source` only contains comments or empty spaces, return
                // an empty range.
                range: if is_src_empty {
                    TextRange::default()
                } else {
                    TextRange::new(
                        0.into(),
                        self.source
                            .len()
                            .try_into()
                            .expect("source length is  bigger than u32 max"),
                    )
                },
            })
        };

        Program {
            ast,
            parse_errors: self.finish(),
        }
    }

    #[inline]
    fn set_ctx(&mut self, ctx: ParserCtxFlags) {
        self.ctx_stack.push(self.ctx);
        self.ctx = ctx;
    }

    #[inline]
    fn clear_ctx(&mut self, ctx: ParserCtxFlags) {
        assert_eq!(self.ctx, ctx);
        self.last_ctx = ctx;
        if let Some(top) = self.ctx_stack.pop() {
            self.ctx = top;
        }
    }

    /// Returns the start position for a node that starts at the current token.
    fn node_start(&self) -> TextSize {
        self.current_range().start()
    }

    fn node_range(&self, start: TextSize) -> TextRange {
        TextRange::new(start, self.last_token_end)
    }

    #[inline]
    fn has_ctx(&self, ctx: ParserCtxFlags) -> bool {
        self.ctx.intersects(ctx)
    }

    /// Moves the parser to the next token. Returns the old current token as an owned value.
    fn next_token(&mut self) -> Spanned {
        let next = self
            .tokens
            .next()
            .unwrap_or_else(|| (Tok::EndOfFile, TextRange::empty(self.source.text_len())));

        let current = std::mem::replace(&mut self.current, next);

        if !matches!(
            current.0,
            // TODO explore including everything up to the dedent as part of the body.
            Tok::Dedent
            // Don't include newlines in the body
            | Tok::Newline
            // TODO(micha): Including the semi feels more correct but it isn't compatible with lalrpop and breaks the
            // formatters semicolon detection. Exclude it for now
            | Tok::Semi
        ) {
            self.last_token_end = current.1.end();
        }

        current
    }

    fn peek_nth(&mut self, offset: usize) -> (TokenKind, TextRange) {
        if offset == 0 {
            self.current_token()
        } else {
            self.tokens.peek_nth(offset - 1).unwrap_or_else(|| {
                (
                    TokenKind::EndOfFile,
                    TextRange::empty(self.source.text_len()),
                )
            })
        }
    }

    #[inline]
    fn current_token(&self) -> (TokenKind, TextRange) {
        (self.current_kind(), self.current_range())
    }

    #[inline]
    fn current_kind(&self) -> TokenKind {
        // TODO: Converting the token kind over and over again can be expensive.
        TokenKind::from_token(&self.current.0)
    }

    #[inline]
    fn current_range(&self) -> TextRange {
        self.current.1
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if !self.at(kind) {
            return false;
        }

        self.next_token();
        true
    }

    /// Bumps the current token assuming it is of the given kind.
    ///
    /// # Panics
    /// If the current token is not of the given kind.
    ///
    /// # Returns
    /// The current token
    fn bump(&mut self, kind: TokenKind) -> Spanned {
        assert_eq!(self.current_kind(), kind);

        self.next_token()
    }

    fn expect(&mut self, expected: TokenKind) -> bool {
        if self.eat(expected) {
            return true;
        }

        let (found, range) = self.current_token();
        self.add_error(ParseErrorType::ExpectedToken { found, expected }, range);
        false
    }

    /// Expects a specific token kind, skipping leading unexpected tokens if needed.
    fn expect_and_recover(&mut self, expected: TokenKind, recover_set: TokenSet) {
        if !self.expect(expected) {
            let expected_set = NEWLINE_EOF_SET
                .union(recover_set)
                .union([expected].as_slice().into());
            // Skip leading unexpected tokens
            let range = self.skip_until(expected_set);
            self.defer_invalid_node_creation = Some(range);

            self.add_error(
                ParseErrorType::OtherError("unexpected tokens".into()),
                range,
            );

            self.eat(expected);
        }
    }

    fn add_error(&mut self, error: ParseErrorType, range: TextRange) {
        self.errors.push(ParseError {
            error,
            location: range,
        });
    }

    /// Skip tokens until [`TokenSet`]. Returns the range of the skipped tokens.
    fn skip_until(&mut self, token_set: TokenSet) -> TextRange {
        let mut final_range = self.current_range();
        while !self.at_ts(token_set) {
            let (_, range) = self.next_token();
            final_range = final_range.cover(range);
        }

        final_range
    }

    fn at(&mut self, kind: TokenKind) -> bool {
        self.current_kind() == kind
    }

    fn at_ts(&mut self, ts: TokenSet) -> bool {
        ts.contains(self.current_kind())
    }

    fn at_expr(&mut self) -> bool {
        self.at_ts(EXPR_SET)
    }

    fn at_simple_stmt(&mut self) -> bool {
        self.at_ts(SIMPLE_STMT_SET2)
    }

    fn at_compound_stmt(&mut self) -> bool {
        self.at_ts(COMPOUND_STMT_SET)
    }

    fn src_text(&self, mut range: TextRange) -> &'src str {
        // This check is to prevent the parser from panicking when using the
        // `parse_expression_starts_at` function with an offset bigger than zero.
        //
        // The parser assumes that the token's range values are smaller than
        // the source length. But, with an offset bigger than zero, it can
        // happen that the token's range values are bigger than the source
        // length, causing the parser to panic when calling this function
        // with such ranges.
        //
        // Therefore, we fix this by creating a new range starting at 0 up to
        // the source length - 1.
        //
        // TODO: Create the proper range here.
        let src_len = self.source.len();
        if range.start().to_usize() > src_len || range.end().to_usize() > src_len {
            range = TextRange::new(0.into(), self.source.text_len() - TextSize::from(1));
        }
        &self.source[range]
    }

    /// Parses elements enclosed within a delimiter pair, such as parentheses, brackets,
    /// or braces.
    ///
    /// Returns the [`TextRange`] of the parsed enclosed elements.
    fn parse_delimited(
        &mut self,
        allow_trailing_delim: bool,
        opening: TokenKind,
        delim: TokenKind,
        closing: TokenKind,
        mut func: impl FnMut(&mut Parser<'src>),
    ) -> TextRange {
        let start_range = self.current_range();
        assert!(self.eat(opening));

        self.parse_separated(
            allow_trailing_delim,
            delim,
            [closing].as_slice(),
            |parser| {
                func(parser);
                // Doesn't matter what range we return here
                TextRange::default()
            },
        );

        let end_range = self.current_range();
        self.expect_and_recover(closing, TokenSet::EMPTY);

        start_range.cover(end_range)
    }

    /// Parses a sequence of elements separated by a delimiter. This function stops
    /// parsing upon encountering any of the tokens in `ending_set`, if it doesn't
    /// encounter the tokens in `ending_set` it stops parsing when seeing the `EOF`
    /// or `Newline` token.
    ///
    /// Returns the last [`TextRange`] of the parsed elements. If none elements are
    /// parsed it returns `None`.
    fn parse_separated(
        &mut self,
        allow_trailing_delim: bool,
        delim: TokenKind,
        ending_set: impl Into<TokenSet>,
        mut func: impl FnMut(&mut Parser<'src>) -> TextRange,
    ) -> Option<TextRange> {
        let ending_set = NEWLINE_EOF_SET.union(ending_set.into());
        let mut final_range = None;

        while !self.at_ts(ending_set) {
            final_range = Some(func(self));

            // exit the loop if a trailing `delim` is not allowed
            if !allow_trailing_delim && ending_set.contains(self.peek_nth(1).0) {
                break;
            }

            if self.at(delim) {
                final_range = Some(self.current_range());
                self.eat(delim);
            } else {
                if self.at_expr() {
                    self.expect(delim);
                } else {
                    break;
                }
            }
        }

        final_range
    }

    fn is_current_token_postfix(&mut self) -> bool {
        matches!(
            self.current_kind(),
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Dot | TokenKind::Async | TokenKind::For
        )
    }

    fn handle_unexpected_indentation(&mut self, stmts: &mut Vec<Stmt>, error_msg: &str) {
        self.bump(TokenKind::Indent);

        self.add_error(
            ParseErrorType::OtherError(error_msg.to_string()),
            self.current_range(),
        );

        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::EndOfFile) {
            let stmt = self.parse_statement();
            stmts.push(stmt);
        }

        assert!(self.eat(TokenKind::Dedent));
    }

    fn parse_statement(&mut self) -> Stmt {
        let start_offset = self.node_start();
        match self.current_kind() {
            TokenKind::If => Stmt::If(self.parse_if_stmt()),
            TokenKind::Try => Stmt::Try(self.parse_try_stmt()),
            TokenKind::For => Stmt::For(self.parse_for_stmt(start_offset)),
            TokenKind::With => Stmt::With(self.parse_with_stmt(start_offset)),
            TokenKind::At => self.parse_decorators(),
            TokenKind::Async => self.parse_async_stmt(),
            TokenKind::While => Stmt::While(self.parse_while_stmt()),
            TokenKind::Def => Stmt::FunctionDef(self.parse_func_def_stmt(vec![], start_offset)),
            TokenKind::Class => Stmt::ClassDef(self.parse_class_def_stmt(vec![], start_offset)),
            TokenKind::Match => Stmt::Match(self.parse_match_stmt()),
            _ => self.parse_simple_stmt_newline(),
        }
    }

    fn parse_match_stmt(&mut self) -> ast::StmtMatch {
        let start_offset = self.node_start();

        self.bump(TokenKind::Match);
        let (subject, _) = self.parse_expr_with_recovery(
            |parser| {
                let (parsed_expr, expr_range) = parser.parse_expr2();
                if parser.at(TokenKind::Comma) {
                    return parser.parse_tuple_expr(
                        parsed_expr.expr,
                        expr_range,
                        Parser::parse_expr2,
                    );
                }
                (parsed_expr, expr_range)
            },
            [TokenKind::Colon].as_slice(),
            "expecting expression after `match` keyword",
        );
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        self.eat(TokenKind::Newline);
        if !self.eat(TokenKind::Indent) {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError(
                    "expected an indented block after `match` statement".to_string(),
                ),
                range,
            );
        }

        let (cases, _) = self.parse_match_cases();

        self.eat(TokenKind::Dedent);

        ast::StmtMatch {
            subject: Box::new(subject.expr),
            cases,
            range: self.node_range(start_offset),
        }
    }

    fn parse_match_case(&mut self) -> ast::MatchCase {
        let start = self.node_start();

        self.bump(TokenKind::Case);
        let (pattern, _) = self.parse_match_patterns();

        let guard = if self.eat(TokenKind::If) {
            let (parsed_expr, _) = self.parse_expr2();
            Some(Box::new(parsed_expr.expr))
        } else {
            None
        };

        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);
        let body = self.parse_body(Clause::Match);

        ast::MatchCase {
            pattern,
            guard,
            body,
            range: self.node_range(start),
        }
    }

    fn parse_match_cases(&mut self) -> (Vec<ast::MatchCase>, TextRange) {
        let mut range = self.current_range();

        if !self.at(TokenKind::Case) {
            self.add_error(
                ParseErrorType::OtherError("expecting `case` block after `match`".to_string()),
                range,
            );
        }

        let mut cases = vec![];
        while self.at(TokenKind::Case) {
            let case = self.parse_match_case();
            range = range.cover(case.range);

            cases.push(case);
        }

        (cases, range)
    }

    fn parse_attr_expr_for_match_pattern(
        &mut self,
        mut lhs: Expr,
        mut lhs_range: TextRange,
    ) -> ExprWithRange {
        loop {
            (lhs, lhs_range) = match self.current_kind() {
                TokenKind::Dot => {
                    let (parsed_expr, range) = self.parse_attribute_expr(lhs, lhs_range);
                    (parsed_expr.expr, range)
                }
                _ => break,
            }
        }

        (lhs.into(), lhs_range)
    }

    fn parse_match_pattern_literal(&mut self) -> (Pattern, TextRange) {
        let (tok, range) = self.next_token();
        match tok {
            Tok::None => (
                Pattern::MatchSingleton(ast::PatternMatchSingleton {
                    value: Singleton::None,
                    range,
                }),
                range,
            ),
            Tok::True => (
                Pattern::MatchSingleton(ast::PatternMatchSingleton {
                    value: Singleton::True,
                    range,
                }),
                range,
            ),
            Tok::False => (
                Pattern::MatchSingleton(ast::PatternMatchSingleton {
                    value: Singleton::False,
                    range,
                }),
                range,
            ),
            tok @ Tok::String { .. } => {
                let (str, str_range) = self.parse_string_expr(tok, range);
                (
                    Pattern::MatchValue(ast::PatternMatchValue {
                        value: Box::new(str.expr),
                        range: str_range,
                    }),
                    str_range,
                )
            }
            Tok::Complex { real, imag } => (
                Pattern::MatchValue(ast::PatternMatchValue {
                    value: Box::new(Expr::NumberLiteral(ast::ExprNumberLiteral {
                        value: Number::Complex { real, imag },
                        range,
                    })),
                    range,
                }),
                range,
            ),
            Tok::Int { value } => (
                Pattern::MatchValue(ast::PatternMatchValue {
                    value: Box::new(Expr::NumberLiteral(ast::ExprNumberLiteral {
                        value: Number::Int(value),
                        range,
                    })),
                    range,
                }),
                range,
            ),
            Tok::Float { value } => (
                Pattern::MatchValue(ast::PatternMatchValue {
                    value: Box::new(Expr::NumberLiteral(ast::ExprNumberLiteral {
                        value: Number::Float(value),
                        range,
                    })),
                    range,
                }),
                range,
            ),
            Tok::Name { name } if self.at(TokenKind::Dot) => {
                let id = Expr::Name(ast::ExprName {
                    id: name,
                    ctx: ExprContext::Load,
                    range,
                });
                let (parsed_expr, range) = self.parse_attr_expr_for_match_pattern(id, range);
                (
                    Pattern::MatchValue(ast::PatternMatchValue {
                        value: Box::new(parsed_expr.expr),
                        range,
                    }),
                    range,
                )
            }
            Tok::Name { name } => (
                Pattern::MatchAs(ast::PatternMatchAs {
                    range,
                    pattern: None,
                    name: if name == "_" {
                        None
                    } else {
                        Some(ast::Identifier { id: name, range })
                    },
                }),
                range,
            ),
            Tok::Minus
                if matches!(
                    self.current_kind(),
                    TokenKind::Int | TokenKind::Float | TokenKind::Complex
                ) =>
            {
                // Since the `Minus` token was consumed `parse_lhs` will not
                // be able to parse an `UnaryOp`, therefore we create the node
                // manually.
                let (parsed_expr, expr_range) = self.parse_lhs();
                let range = range.cover(expr_range);
                (
                    Pattern::MatchValue(ast::PatternMatchValue {
                        value: Box::new(Expr::UnaryOp(ast::ExprUnaryOp {
                            range,
                            op: UnaryOp::USub,
                            operand: Box::new(parsed_expr.expr),
                        })),
                        range,
                    }),
                    range,
                )
            }
            kind => {
                const RECOVERY_SET: TokenSet =
                    TokenSet::new(&[TokenKind::Colon]).union(NEWLINE_EOF_SET);
                self.add_error(
                    ParseErrorType::InvalidMatchPatternLiteral {
                        pattern: kind.into(),
                    },
                    range,
                );
                self.skip_until(RECOVERY_SET);
                (
                    Pattern::Invalid(ast::PatternMatchInvalid {
                        value: self.src_text(range).into(),
                        range,
                    }),
                    range.cover_offset(self.current_range().start()),
                )
            }
        }
    }

    fn parse_delimited_match_pattern(&mut self) -> (Pattern, TextRange) {
        let mut range = self.current_range();

        let is_paren = self.at(TokenKind::Lpar);
        let is_bracket = self.at(TokenKind::Lsqb);

        let closing = if is_paren {
            self.eat(TokenKind::Lpar);
            TokenKind::Rpar
        } else {
            self.eat(TokenKind::Lsqb);
            TokenKind::Rsqb
        };

        if matches!(self.current_kind(), TokenKind::Newline | TokenKind::Colon) {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "missing `{}`",
                    if is_paren { ')' } else { ']' }
                )),
                range,
            );
        }

        if self.at(closing) {
            range = range.cover(self.current_range());
            self.eat(closing);

            return (
                Pattern::MatchSequence(ast::PatternMatchSequence {
                    patterns: vec![],
                    range,
                }),
                range,
            );
        }

        let (mut pattern, pattern_range) = self.parse_match_pattern();

        if is_bracket || self.at(TokenKind::Comma) {
            (pattern, _) = self.parse_sequence_match_pattern(pattern, pattern_range, closing);
        }

        range = range.cover(self.current_range());
        self.expect_and_recover(closing, TokenSet::EMPTY);

        if let Pattern::MatchSequence(mut sequence) = pattern {
            // Update the range to include the parenthesis or brackets
            sequence.range = range;
            (Pattern::MatchSequence(sequence), range)
        } else {
            (pattern, range)
        }
    }

    fn parse_sequence_match_pattern(
        &mut self,
        first_elt: Pattern,
        elt_range: TextRange,
        ending: TokenKind,
    ) -> (Pattern, TextRange) {
        // In case of the match sequence only having one element, we need to cover
        // the range of the comma.
        let mut final_range = elt_range.cover(self.current_range());
        self.eat(TokenKind::Comma);
        let mut patterns = vec![first_elt];

        let range = self.parse_separated(true, TokenKind::Comma, [ending].as_slice(), |parser| {
            let (pattern, pattern_range) = parser.parse_match_pattern();
            patterns.push(pattern);
            pattern_range
        });
        final_range = final_range.cover(range.unwrap_or(final_range));

        (
            Pattern::MatchSequence(ast::PatternMatchSequence {
                patterns,
                range: final_range,
            }),
            final_range,
        )
    }

    fn parse_match_pattern_lhs(&mut self) -> (Pattern, TextRange) {
        let (mut lhs, mut range) = match self.current_kind() {
            TokenKind::Lbrace => self.parse_match_pattern_mapping(),
            TokenKind::Star => self.parse_match_pattern_star(),
            TokenKind::Lpar | TokenKind::Lsqb => self.parse_delimited_match_pattern(),
            _ => self.parse_match_pattern_literal(),
        };

        if self.at(TokenKind::Lpar) {
            (lhs, range) = self.parse_match_pattern_class(lhs, range);
        }

        if self.at(TokenKind::Plus) || self.at(TokenKind::Minus) {
            let (op_kind, _) = self.next_token();

            let (lhs_value, lhs_range) = if let Pattern::MatchValue(lhs) = lhs {
                if !lhs.value.is_literal_expr() && !matches!(lhs.value.as_ref(), Expr::UnaryOp(_)) {
                    self.add_error(
                        ParseErrorType::OtherError(format!(
                            "invalid `{}` expression for match pattern",
                            self.src_text(lhs.range)
                        )),
                        lhs.range,
                    );
                }
                (lhs.value, lhs.range)
            } else {
                self.add_error(
                    ParseErrorType::OtherError("invalid lhs pattern".to_string()),
                    range,
                );
                (
                    Box::new(Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(range).into(),
                        range,
                    })),
                    range,
                )
            };

            let (rhs_pattern, rhs_range) = self.parse_match_pattern_lhs();
            let (rhs_value, rhs_range) = if let Pattern::MatchValue(rhs) = rhs_pattern {
                if !rhs.value.is_literal_expr() {
                    self.add_error(
                        ParseErrorType::OtherError(format!(
                            "invalid `{}` expression for match pattern",
                            self.src_text(rhs_range)
                        )),
                        rhs_range,
                    );
                }
                (rhs.value, rhs.range)
            } else {
                self.add_error(
                    ParseErrorType::OtherError("invalid rhs pattern".to_string()),
                    rhs_range,
                );
                (
                    Box::new(Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(rhs_range).into(),
                        range: rhs_range,
                    })),
                    rhs_range,
                )
            };

            if matches!(
                rhs_value.as_ref(),
                Expr::UnaryOp(ast::ExprUnaryOp {
                    op: UnaryOp::USub,
                    ..
                })
            ) {
                self.add_error(
                    ParseErrorType::OtherError(
                        "`-` not allowed in rhs of match pattern".to_string(),
                    ),
                    rhs_range,
                );
            }

            let op = if matches!(op_kind, Tok::Plus) {
                Operator::Add
            } else {
                Operator::Sub
            };
            let range = lhs_range.cover(rhs_range);
            return (
                Pattern::MatchValue(ast::PatternMatchValue {
                    value: Box::new(Expr::BinOp(ast::ExprBinOp {
                        left: lhs_value,
                        op,
                        right: rhs_value,
                        range,
                    })),
                    range,
                }),
                range,
            );
        }

        (lhs, range)
    }

    fn parse_match_pattern(&mut self) -> (Pattern, TextRange) {
        let (mut lhs, mut range) = self.parse_match_pattern_lhs();

        if self.at(TokenKind::Vbar) {
            let mut patterns = vec![lhs];

            while self.eat(TokenKind::Vbar) {
                let (pattern, pattern_range) = self.parse_match_pattern_lhs();
                range = range.cover(pattern_range);
                patterns.push(pattern);
            }

            lhs = Pattern::MatchOr(ast::PatternMatchOr { range, patterns });
        }

        if self.eat(TokenKind::As) {
            let ident = self.parse_identifier();
            range = range.cover(ident.range);
            lhs = Pattern::MatchAs(ast::PatternMatchAs {
                range,
                name: Some(ident),
                pattern: Some(Box::new(lhs)),
            });
        }

        (lhs, range)
    }

    fn parse_match_patterns(&mut self) -> (Pattern, TextRange) {
        let (pattern, range) = self.parse_match_pattern();

        if self.at(TokenKind::Comma) {
            return self.parse_sequence_match_pattern(pattern, range, TokenKind::Colon);
        }

        (pattern, range)
    }

    fn parse_match_pattern_star(&mut self) -> (Pattern, TextRange) {
        let mut range = self.current_range();
        self.eat(TokenKind::Star);

        let ident = self.parse_identifier();

        range = range.cover(ident.range);
        (
            Pattern::MatchStar(ast::PatternMatchStar {
                range,
                name: if ident.is_valid() && ident.id == "_" {
                    None
                } else {
                    Some(ident)
                },
            }),
            range,
        )
    }

    fn parse_match_pattern_class(
        &mut self,
        cls: Pattern,
        mut cls_range: TextRange,
    ) -> (Pattern, TextRange) {
        let mut patterns = vec![];
        let mut keywords = vec![];
        let mut has_seen_pattern = false;
        let mut has_seen_keyword_pattern = false;

        let args_range = self.parse_delimited(
            true,
            TokenKind::Lpar,
            TokenKind::Comma,
            TokenKind::Rpar,
            |parser| {
                let (pattern, pattern_range) = parser.parse_match_pattern();

                if parser.eat(TokenKind::Equal) {
                    has_seen_pattern = false;
                    has_seen_keyword_pattern = true;

                    if let Pattern::MatchAs(ast::PatternMatchAs {
                        name: Some(attr),
                        range,
                        ..
                    }) = pattern
                    {
                        let (pattern, _) = parser.parse_match_pattern();

                        keywords.push(ast::PatternKeyword {
                            attr,
                            pattern,
                            range: range.cover_offset(parser.current_range().start()),
                        });
                    } else {
                        parser.skip_until(END_EXPR_SET);
                        parser.add_error(
                            ParseErrorType::OtherError(format!(
                                "`{}` not valid keyword pattern",
                                parser.src_text(pattern_range)
                            )),
                            pattern_range,
                        );
                    }
                } else {
                    has_seen_pattern = true;
                    patterns.push(pattern);
                }

                if has_seen_keyword_pattern && has_seen_pattern {
                    parser.add_error(
                        ParseErrorType::OtherError(
                            "pattern not allowed after keyword pattern".to_string(),
                        ),
                        pattern_range,
                    );
                }
            },
        );

        let cls = match cls {
            Pattern::MatchAs(ast::PatternMatchAs {
                name: Some(ident), ..
            }) => {
                cls_range = ident.range;
                if ident.is_valid() {
                    Box::new(Expr::Name(ast::ExprName {
                        id: ident.id,
                        ctx: ExprContext::Load,
                        range: cls_range,
                    }))
                } else {
                    Box::new(Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(cls_range).into(),
                        range: cls_range,
                    }))
                }
            }
            Pattern::MatchValue(ast::PatternMatchValue {
                value,
                range: value_range,
            }) if matches!(value.as_ref(), Expr::Attribute(_)) => {
                cls_range = value_range;
                value
            }
            _ => {
                self.add_error(
                    ParseErrorType::OtherError(format!(
                        "`{}` invalid pattern match class",
                        self.src_text(cls_range)
                    )),
                    cls_range,
                );
                Box::new(Expr::Invalid(ast::ExprInvalid {
                    value: self.src_text(cls_range).into(),
                    range: cls_range,
                }))
            }
        };

        let range = cls_range.cover(args_range);
        (
            Pattern::MatchClass(ast::PatternMatchClass {
                cls,
                arguments: ast::PatternArguments {
                    patterns,
                    keywords,
                    range: args_range,
                },
                range,
            }),
            range,
        )
    }

    fn parse_match_pattern_mapping(&mut self) -> (Pattern, TextRange) {
        let mut keys = vec![];
        let mut patterns = vec![];
        let mut rest = None;

        let range = self.parse_delimited(
            true,
            TokenKind::Lbrace,
            TokenKind::Comma,
            TokenKind::Rbrace,
            |parser| {
                if parser.eat(TokenKind::DoubleStar) {
                    rest = Some(parser.parse_identifier());
                } else {
                    let (pattern, pattern_range) = parser.parse_match_pattern_lhs();
                    let key = match pattern {
                        Pattern::MatchValue(ast::PatternMatchValue { value, .. }) => *value,
                        Pattern::MatchSingleton(ast::PatternMatchSingleton { value, range }) => {
                            match value {
                                Singleton::None => {
                                    Expr::NoneLiteral(ast::ExprNoneLiteral { range })
                                }
                                Singleton::True => Expr::BooleanLiteral(ast::ExprBooleanLiteral {
                                    value: true,
                                    range,
                                }),
                                Singleton::False => Expr::BooleanLiteral(ast::ExprBooleanLiteral {
                                    value: false,
                                    range,
                                }),
                            }
                        }
                        _ => {
                            parser.add_error(
                                ParseErrorType::OtherError(format!(
                                    "invalid mapping pattern key `{}`",
                                    parser.src_text(pattern_range)
                                )),
                                pattern_range,
                            );
                            Expr::Invalid(ast::ExprInvalid {
                                value: parser.src_text(pattern_range).into(),
                                range: pattern_range,
                            })
                        }
                    };
                    keys.push(key);

                    parser.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

                    let (pattern, _) = parser.parse_match_pattern();
                    patterns.push(pattern);
                }
            },
        );

        (
            Pattern::MatchMapping(ast::PatternMatchMapping {
                range,
                keys,
                patterns,
                rest,
            }),
            range,
        )
    }

    fn parse_async_stmt(&mut self) -> Stmt {
        let async_start = self.node_start();
        self.bump(TokenKind::Async);

        match self.current_kind() {
            TokenKind::Def => {
                let mut function = self.parse_func_def_stmt(vec![], async_start);
                function.is_async = true;
                Stmt::FunctionDef(function)
            }
            TokenKind::With => {
                let mut with_stmt = self.parse_with_stmt(async_start);
                with_stmt.is_async = true;
                Stmt::With(with_stmt)
            }
            TokenKind::For => {
                let mut for_stmt = self.parse_for_stmt(async_start);
                for_stmt.is_async = true;
                Stmt::For(for_stmt)
            }
            kind => {
                // Although this statement is not a valid `async` statement,
                // we still parse it.
                self.add_error(ParseErrorType::StmtIsNotAsync(kind), self.current_range());
                self.parse_statement()
            }
        }
    }

    fn parse_while_stmt(&mut self) -> ast::StmtWhile {
        let while_start = self.node_start();
        self.bump(TokenKind::While);

        let (test, _) = self.parse_expr_with_recovery(
            Parser::parse_expr2,
            [TokenKind::Colon].as_slice(),
            "expecting expression after `while` keyword",
        );
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let body = self.parse_body(Clause::While);

        let orelse = if self.eat(TokenKind::Else) {
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            let else_body = self.parse_body(Clause::Else);
            else_body
        } else {
            vec![]
        };

        ast::StmtWhile {
            test: Box::new(test.expr),
            body,
            orelse,
            range: self.node_range(while_start),
        }
    }

    fn parse_for_stmt(&mut self, for_start: TextSize) -> ast::StmtFor {
        self.bump(TokenKind::For);

        self.set_ctx(ParserCtxFlags::FOR_TARGET);
        let (mut target, _) = self.parse_expr_with_recovery(
            Parser::parse_exprs,
            [TokenKind::In, TokenKind::Colon].as_slice(),
            "expecting expression after `for` keyword",
        );
        self.clear_ctx(ParserCtxFlags::FOR_TARGET);

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        self.expect_and_recover(TokenKind::In, TokenSet::new(&[TokenKind::Colon]));

        let (iter, _) = self.parse_expr_with_recovery(
            Parser::parse_exprs,
            EXPR_SET.union([TokenKind::Colon, TokenKind::Indent].as_slice().into()),
            "expecting an expression after `in` keyword",
        );
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let body = self.parse_body(Clause::For);

        let orelse = if self.eat(TokenKind::Else) {
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            let else_body = self.parse_body(Clause::Else);
            else_body
        } else {
            vec![]
        };

        ast::StmtFor {
            target: Box::new(target.expr),
            iter: Box::new(iter.expr),
            is_async: false,
            body,
            orelse,
            range: self.node_range(for_start),
        }
    }

    fn parse_try_stmt(&mut self) -> ast::StmtTry {
        let try_start = self.node_start();
        self.bump(TokenKind::Try);
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let mut is_star = false;
        let mut has_except = false;
        let mut has_finally = false;

        let try_body = self.parse_body(Clause::Try);

        let mut handlers = vec![];
        loop {
            let except_start = self.node_start();
            if self.eat(TokenKind::Except) {
                has_except = true;
            } else {
                break;
            }

            is_star = self.eat(TokenKind::Star);

            let type_ = if self.at(TokenKind::Colon) && !is_star {
                None
            } else {
                let (parsed_expr, expr_range) = self.parse_exprs();
                if !parsed_expr.is_parenthesized && matches!(parsed_expr.expr, Expr::Tuple(_)) {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "multiple exception types must be parenthesized".to_string(),
                        ),
                        expr_range,
                    );
                }
                Some(Box::new(parsed_expr.expr))
            };

            let name = if self.eat(TokenKind::As) {
                Some(self.parse_identifier())
            } else {
                None
            };

            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            let except_body = self.parse_body(Clause::Except);

            let except_range = self.node_range(except_start);
            handlers.push(ExceptHandler::ExceptHandler(
                ast::ExceptHandlerExceptHandler {
                    type_,
                    name,
                    body: except_body,
                    range: except_range,
                },
            ));

            if !self.at(TokenKind::Except) {
                break;
            }
        }

        let orelse = if self.eat(TokenKind::Else) {
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            self.parse_body(Clause::Else)
        } else {
            vec![]
        };

        let finalbody = if self.eat(TokenKind::Finally) {
            has_finally = true;
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            self.parse_body(Clause::Finally)
        } else {
            vec![]
        };

        if !has_except && !has_finally {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError(
                    "expecting `except` or `finally` after `try` block".to_string(),
                ),
                range,
            );
        }

        let range = self.node_range(try_start);

        ast::StmtTry {
            body: try_body,
            handlers,
            orelse,
            finalbody,
            is_star,
            range,
        }
    }

    fn parse_decorators(&mut self) -> Stmt {
        let start_offset = self.node_start();

        let mut decorators = vec![];

        while self.at(TokenKind::At) {
            let decorator_start = self.node_start();
            self.bump(TokenKind::At);

            let (parsed_expr, _) = self.parse_expr2();
            decorators.push(ast::Decorator {
                expression: parsed_expr.expr,
                range: self.node_range(decorator_start),
            });

            self.expect(TokenKind::Newline);
        }

        match self.current_kind() {
            TokenKind::Def => Stmt::FunctionDef(self.parse_func_def_stmt(decorators, start_offset)),
            TokenKind::Class => Stmt::ClassDef(self.parse_class_def_stmt(decorators, start_offset)),
            TokenKind::Async if self.peek_nth(1).0 == TokenKind::Def => {
                self.bump(TokenKind::Async);

                let mut func = self.parse_func_def_stmt(decorators, start_offset);
                func.is_async = true;

                Stmt::FunctionDef(func)
            }
            _ => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "expected class, function definition or async function definition after decorator".to_string(),
                    ),
                    self.current_range(),
                );
                self.parse_statement()
            }
        }
    }

    fn parse_func_def_stmt(
        &mut self,
        decorator_list: Vec<ast::Decorator>,
        start_offset: TextSize,
    ) -> ast::StmtFunctionDef {
        self.bump(TokenKind::Def);
        let name = self.parse_identifier();
        let type_params = if self.at(TokenKind::Lsqb) {
            Some(self.parse_type_params())
        } else {
            None
        };

        let lpar_range = self.current_range();
        self.expect_and_recover(
            TokenKind::Lpar,
            EXPR_SET.union(
                [TokenKind::Colon, TokenKind::Rarrow, TokenKind::Comma]
                    .as_slice()
                    .into(),
            ),
        );

        let mut parameters = self.parse_parameters(FunctionKind::FunctionDef);

        let rpar_range = self.current_range();

        self.expect_and_recover(
            TokenKind::Rpar,
            SIMPLE_STMT_SET
                .union(COMPOUND_STMT_SET)
                .union([TokenKind::Colon, TokenKind::Rarrow].as_slice().into()),
        );

        parameters.range = lpar_range.cover(rpar_range);

        let returns = if self.eat(TokenKind::Rarrow) {
            let (returns, range) = self.parse_exprs();
            if !returns.is_parenthesized && matches!(returns.expr, Expr::Tuple(_)) {
                self.add_error(
                    ParseErrorType::OtherError(
                        "multiple return types must be parenthesized".to_string(),
                    ),
                    range,
                );
            }
            Some(Box::new(returns.expr))
        } else {
            None
        };

        self.expect_and_recover(
            TokenKind::Colon,
            SIMPLE_STMT_SET
                .union(COMPOUND_STMT_SET)
                .union([TokenKind::Rarrow].as_slice().into()),
        );

        let body = self.parse_body(Clause::FunctionDef);

        ast::StmtFunctionDef {
            name,
            type_params,
            parameters: Box::new(parameters),
            body,
            decorator_list,
            is_async: false,
            returns,
            range: self.node_range(start_offset),
        }
    }

    fn parse_class_def_stmt(
        &mut self,
        decorator_list: Vec<ast::Decorator>,
        start_offset: TextSize,
    ) -> ast::StmtClassDef {
        self.bump(TokenKind::Class);

        let name = self.parse_identifier();
        let type_params = if self.at(TokenKind::Lsqb) {
            Some(Box::new(self.parse_type_params()))
        } else {
            None
        };
        let arguments = if self.at(TokenKind::Lpar) {
            Some(Box::new(self.parse_arguments()))
        } else {
            None
        };

        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let body = self.parse_body(Clause::Class);

        ast::StmtClassDef {
            range: self.node_range(start_offset),
            decorator_list,
            name,
            type_params,
            arguments,
            body,
        }
    }

    fn parse_with_item(&mut self) -> ast::WithItem {
        let (context_expr, mut range) = self.parse_expr();
        match context_expr.expr {
            Expr::Starred(_) => {
                self.add_error(
                    ParseErrorType::OtherError("starred expression not allowed".into()),
                    range,
                );
            }
            Expr::NamedExpr(_) if !context_expr.is_parenthesized => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "unparenthesized named expression not allowed".into(),
                    ),
                    range,
                );
            }
            _ => {}
        }

        let optional_vars = if self.eat(TokenKind::As) {
            let (mut target, target_range) = self.parse_expr();
            range = range.cover(target_range);

            if matches!(target.expr, Expr::BoolOp(_) | Expr::Compare(_)) {
                // Should we make `target` an `Expr::Invalid` here?
                self.add_error(
                    ParseErrorType::OtherError(format!(
                        "expression `{target:?}` not allowed in `with` statement"
                    )),
                    target_range,
                );
            }

            helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

            Some(Box::new(target.expr))
        } else {
            None
        };

        ast::WithItem {
            range,
            context_expr: context_expr.expr,
            optional_vars,
        }
    }

    fn parse_with_items(&mut self) -> Vec<ast::WithItem> {
        let mut items = vec![];

        if !self.at_expr() {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError("expecting expression after `with` keyword".to_string()),
                range,
            );
            return items;
        }

        let has_seen_lpar = self.at(TokenKind::Lpar);

        // Consider the two `WithItem` examples below:
        //      1) `(a) as A`
        //      2) `(a)`
        //
        // In the first example, the `item` contains a parenthesized expression,
        // while the second example is a parenthesized `WithItem`. This situation
        // introduces ambiguity during parsing. When encountering an opening parenthesis
        // `(,` the parser may initially assume it's parsing a parenthesized `WithItem`.
        // However, this assumption doesn't hold for the first case, `(a) as A`, where
        // `(a)` represents a parenthesized expression.
        //
        // To disambiguate, the following heuristic was created. First, assume we're
        // parsing an expression, then we look for the following tokens:
        //      i) `as` keyword outside parenthesis
        //      ii) `,` outside or inside parenthesis
        //      iii) `:=` inside an 1-level nested parenthesis
        //      iv) `*` inside an 1-level nested parenthesis, representing a starred
        //         expression
        //
        // If we find case i we treat it as in case 1. For case ii, we only treat it as in
        // case 1 if the comma is outside of parenthesis and we've seen an `Rpar` or `Lpar`
        // before the comma.
        // Cases iii and iv are special cases, when we find them, we treat it as in case 2.
        // The reason for this is that the resulting AST node needs to be a tuple for cases
        // iii and iv instead of multiple `WithItem`s. For example, `with (a, b := 0, c): ...`
        // will be parsed as one `WithItem` containing a tuple, instead of three different `WithItem`s.
        let mut treat_it_as_expr = true;
        if has_seen_lpar {
            let mut index = 1;
            let mut paren_nesting = 1;
            let mut ignore_comma_check = false;
            let mut has_seen_rpar = false;
            let mut has_seen_colon_equal = false;
            let mut has_seen_star = false;
            let mut prev_token = self.current_kind();
            loop {
                let (kind, _) = self.peek_nth(index);
                match kind {
                    TokenKind::Lpar => {
                        paren_nesting += 1;
                    }
                    TokenKind::Rpar => {
                        paren_nesting -= 1;
                        has_seen_rpar = true;
                    }
                    // Check for `:=` inside an 1-level nested parens, e.g. `with (a, b := c): ...`
                    TokenKind::ColonEqual if paren_nesting == 1 => {
                        treat_it_as_expr = true;
                        ignore_comma_check = true;
                        has_seen_colon_equal = true;
                    }
                    // Check for starred expressions inside an 1-level nested parens,
                    // e.g. `with (a, *b): ...`
                    TokenKind::Star if paren_nesting == 1 && !LITERAL_SET.contains(prev_token) => {
                        treat_it_as_expr = true;
                        ignore_comma_check = true;
                        has_seen_star = true;
                    }
                    // Check for `as` keyword outside parens
                    TokenKind::As => {
                        treat_it_as_expr = paren_nesting == 0;
                        ignore_comma_check = true;
                    }
                    TokenKind::Comma if !ignore_comma_check => {
                        // If the comma is outside of parens, treat it as an expression
                        // if we've seen `(` and `)`.
                        if paren_nesting == 0 {
                            treat_it_as_expr = has_seen_lpar && has_seen_rpar;
                        } else if !has_seen_star && !has_seen_colon_equal {
                            treat_it_as_expr = false;
                        }
                    }
                    TokenKind::Colon | TokenKind::Newline => break,
                    _ => {}
                }

                index += 1;
                prev_token = kind;
            }
        }

        if !treat_it_as_expr && has_seen_lpar {
            self.eat(TokenKind::Lpar);
        }

        let ending = if has_seen_lpar && treat_it_as_expr {
            [TokenKind::Colon]
        } else {
            [TokenKind::Rpar]
        };
        self.parse_separated(
            // Only allow a trailing delimiter if we've seen a `(`.
            has_seen_lpar,
            TokenKind::Comma,
            ending.as_slice(),
            |parser| {
                let item = parser.parse_with_item();
                let range = item.range;
                items.push(item);
                range
            },
        );
        // Special-case: if we have a parenthesized `WithItem` that was parsed as
        // an expression, then the item should _exclude_ the outer parentheses in
        // its range. For example:
        // ```python
        // with (a := 0): pass
        // with (*a): pass
        // with (a): pass
        // with (1 + 2): pass
        // ```
        // In this case, the `(` and `)` are part of the `with` statement.
        // The exception is when `WithItem` is an `()` (empty tuple).
        if items.len() == 1 {
            let with_item = items.last_mut().unwrap();
            if treat_it_as_expr
                && with_item.optional_vars.is_none()
                && self.last_ctx.contains(ParserCtxFlags::PARENTHESIZED_EXPR)
                && !matches!(with_item.context_expr, Expr::Tuple(_))
            {
                with_item.range = with_item.range.add_start(1.into()).sub_end(1.into());
            }
        }

        if !treat_it_as_expr && has_seen_lpar {
            self.expect_and_recover(TokenKind::Rpar, TokenSet::new(&[TokenKind::Colon]));
        }

        items
    }

    fn parse_with_stmt(&mut self, start_offset: TextSize) -> ast::StmtWith {
        self.bump(TokenKind::With);

        let items = self.parse_with_items();
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let body = self.parse_body(Clause::With);

        ast::StmtWith {
            items,
            body,
            is_async: false,
            range: self.node_range(start_offset),
        }
    }

    fn parse_assign_stmt(&mut self, target: ParsedExpr, start: TextSize) -> ast::StmtAssign {
        let mut targets = vec![target.expr];
        let (mut value, _) = self.parse_exprs();

        while self.eat(TokenKind::Equal) {
            let (mut parsed_expr, _) = self.parse_exprs();

            std::mem::swap(&mut value, &mut parsed_expr);

            targets.push(parsed_expr.expr);
        }

        targets
            .iter_mut()
            .for_each(|target| helpers::set_expr_ctx(target, ExprContext::Store));

        if !targets.iter().all(helpers::is_valid_assignment_target) {
            targets
                .iter()
                .filter(|target| !helpers::is_valid_assignment_target(target))
                .for_each(|target| self.add_error(ParseErrorType::AssignmentError, target.range()));
        }

        ast::StmtAssign {
            targets,
            value: Box::new(value.expr),
            range: self.node_range(start),
        }
    }

    fn parse_ann_assign_stmt(
        &mut self,
        mut target: ParsedExpr,
        start: TextSize,
    ) -> ast::StmtAnnAssign {
        if !helpers::is_valid_assignment_target(&target.expr) {
            self.add_error(ParseErrorType::AssignmentError, target.expr.range());
        }

        if matches!(target.expr, Expr::Tuple(_)) {
            self.add_error(
                ParseErrorType::OtherError(
                    "only single target (not tuple) can be annotated".into(),
                ),
                target.expr.range(),
            );
        }

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        let simple = matches!(target.expr, Expr::Name(_)) && !target.is_parenthesized;
        let (annotation, _) = self.parse_exprs();

        if matches!(annotation.expr, Expr::Tuple(_)) && !annotation.is_parenthesized {
            self.add_error(
                ParseErrorType::OtherError("annotation cannot be unparenthesized".into()),
                annotation.expr.range(),
            );
        }

        let value = if self.eat(TokenKind::Equal) {
            let (value, _) = self.parse_exprs();

            Some(Box::new(value.expr))
        } else {
            None
        };

        ast::StmtAnnAssign {
            target: Box::new(target.expr),
            annotation: Box::new(annotation.expr),
            value,
            simple,
            range: self.node_range(start),
        }
    }

    fn parse_aug_assign_stmt(
        &mut self,
        mut target: ParsedExpr,
        op: Operator,
        start: TextSize,
    ) -> ast::StmtAugAssign {
        // Consume the operator
        // FIXME(micha): assert that it is an agumented assign token
        self.next_token();

        if !helpers::is_valid_aug_assignment_target(&target.expr) {
            self.add_error(ParseErrorType::AugAssignmentError, target.expr.range());
        }

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        let (value, _) = self.parse_exprs();

        ast::StmtAugAssign {
            target: Box::new(target.expr),
            op,
            value: Box::new(value.expr),
            range: self.node_range(start),
        }
    }

    fn parse_simple_stmt_newline(&mut self) -> Stmt {
        let stmt = self.parse_simple_stmt();

        self.last_ctx = ParserCtxFlags::empty();
        let has_eaten_semicolon = self.eat(TokenKind::Semi);
        let has_eaten_newline = self.eat(TokenKind::Newline);

        if !has_eaten_newline && !has_eaten_semicolon && self.at_simple_stmt() {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::SimpleStmtsInSameLine,
                stmt.range().cover(range),
            );
        }

        if !has_eaten_newline && self.at_compound_stmt() {
            // Avoid create `SimpleStmtAndCompoundStmtInSameLine` error when the
            // current node is `Expr::Invalid`. Example of when this may happen:
            // ```python
            // ! def x(): ...
            // ```
            // The `!` (an unexpected token) will be parsed as `Expr::Invalid`.
            if let Stmt::Expr(expr) = &stmt {
                if let Expr::Invalid(_) = expr.value.as_ref() {
                    return stmt;
                }
            }

            self.add_error(
                ParseErrorType::SimpleStmtAndCompoundStmtInSameLine,
                stmt.range().cover(self.current_range()),
            );
        }

        stmt
    }

    fn parse_simple_stmts(&mut self) -> Vec<Stmt> {
        let mut stmts = vec![];
        let start = self.node_start();

        loop {
            stmts.push(self.parse_simple_stmt());

            if !self.eat(TokenKind::Semi) {
                if self.at_simple_stmt() {
                    for stmt in &stmts {
                        self.add_error(ParseErrorType::SimpleStmtsInSameLine, stmt.range());
                    }
                } else {
                    break;
                }
            }

            if !self.at_simple_stmt() {
                break;
            }
        }

        if !self.eat(TokenKind::Newline) && self.at_compound_stmt() {
            self.add_error(
                ParseErrorType::SimpleStmtAndCompoundStmtInSameLine,
                self.node_range(start),
            );
        }

        stmts
    }

    fn parse_simple_stmt(&mut self) -> Stmt {
        match self.current_kind() {
            TokenKind::Del => Stmt::Delete(self.parse_del_stmt()),
            TokenKind::Pass => Stmt::Pass(self.parse_pass_stmt()),
            TokenKind::Break => Stmt::Break(self.parse_break_stmt()),
            TokenKind::Raise => Stmt::Raise(self.parse_raise_stmt()),
            TokenKind::Assert => Stmt::Assert(self.parse_assert_stmt()),
            TokenKind::Global => Stmt::Global(self.parse_global_stmt()),
            TokenKind::Import => Stmt::Import(self.parse_import_stmt()),
            TokenKind::Return => Stmt::Return(self.parse_return_stmt()),
            TokenKind::From => Stmt::ImportFrom(self.parse_import_from_stmt()),
            TokenKind::Continue => Stmt::Continue(self.parse_continue_stmt()),
            TokenKind::Nonlocal => Stmt::Nonlocal(self.parse_nonlocal_stmt()),
            TokenKind::Type => Stmt::TypeAlias(self.parse_type_alias_stmt()),
            TokenKind::EscapeCommand if self.mode == Mode::Ipython => {
                Stmt::IpyEscapeCommand(self.parse_ipython_escape_command_stmt())
            }
            _ => {
                let start = self.node_start();
                let (parsed_expr, range) = self.parse_exprs();

                if self.eat(TokenKind::Equal) {
                    Stmt::Assign(self.parse_assign_stmt(parsed_expr, start))
                } else if self.eat(TokenKind::Colon) {
                    Stmt::AnnAssign(self.parse_ann_assign_stmt(parsed_expr, start))
                } else if let Ok(op) = Operator::try_from(self.current_kind()) {
                    Stmt::AugAssign(self.parse_aug_assign_stmt(parsed_expr, op, start))
                } else if self.mode == Mode::Ipython && self.at(TokenKind::Question) {
                    let mut kind = IpyEscapeKind::Help;

                    self.eat(TokenKind::Question);
                    if self.at(TokenKind::Question) {
                        kind = IpyEscapeKind::Help2;
                        self.eat(TokenKind::Question);
                    }

                    Stmt::IpyEscapeCommand(ast::StmtIpyEscapeCommand {
                        value: self.src_text(range).to_string(),
                        kind,
                        range: self.node_range(start),
                    })
                } else {
                    Stmt::Expr(ast::StmtExpr {
                        value: Box::new(parsed_expr.expr),
                        range: self.node_range(start),
                    })
                }
            }
        }
    }

    fn parse_ipython_escape_command_stmt(&mut self) -> ast::StmtIpyEscapeCommand {
        let start = self.node_start();
        let (Tok::IpyEscapeCommand { value, kind }, _) = self.bump(TokenKind::EscapeCommand) else {
            unreachable!()
        };

        ast::StmtIpyEscapeCommand {
            range: self.node_range(start),
            kind,
            value,
        }
    }

    fn parse_pass_stmt(&mut self) -> ast::StmtPass {
        let start = self.node_start();
        self.bump(TokenKind::Pass);
        ast::StmtPass {
            range: self.node_range(start),
        }
    }

    fn parse_continue_stmt(&mut self) -> ast::StmtContinue {
        let start = self.node_start();
        self.bump(TokenKind::Continue);
        ast::StmtContinue {
            range: self.node_range(start),
        }
    }

    fn parse_break_stmt(&mut self) -> ast::StmtBreak {
        let start = self.node_start();
        self.bump(TokenKind::Break);
        ast::StmtBreak {
            range: self.node_range(start),
        }
    }

    /// Parses a delete statement.
    ///
    /// # Panics
    /// If the parser isn't positioned at a `del` token.
    fn parse_del_stmt(&mut self) -> ast::StmtDelete {
        let start = self.node_start();

        self.bump(TokenKind::Del);
        let mut targets = vec![];

        self.parse_separated(
            true,
            TokenKind::Comma,
            [TokenKind::Newline].as_slice(),
            |parser| {
                let (mut target, target_range) = parser.parse_expr();
                helpers::set_expr_ctx(&mut target.expr, ExprContext::Del);

                if matches!(target.expr, Expr::BoolOp(_) | Expr::Compare(_)) {
                    // Should we make `target` an `Expr::Invalid` here?
                    parser.add_error(
                        ParseErrorType::OtherError(format!(
                            "`{}` not allowed in `del` statement",
                            parser.src_text(target_range)
                        )),
                        target_range,
                    );
                }
                targets.push(target.expr);
                target_range
            },
        );

        ast::StmtDelete {
            targets,
            range: self.node_range(start),
        }
    }

    fn parse_assert_stmt(&mut self) -> ast::StmtAssert {
        let start = self.node_start();
        self.bump(TokenKind::Assert);

        let (test, _) = self.parse_expr();

        let msg = if self.eat(TokenKind::Comma) {
            let (msg, _) = self.parse_expr();

            Some(Box::new(msg.expr))
        } else {
            None
        };

        ast::StmtAssert {
            test: Box::new(test.expr),
            msg,
            range: self.node_range(start),
        }
    }

    fn parse_global_stmt(&mut self) -> ast::StmtGlobal {
        let start = self.node_start();
        self.bump(TokenKind::Global);

        let mut names = vec![];
        self.parse_separated(
            false,
            TokenKind::Comma,
            [TokenKind::Newline].as_slice(),
            |parser| {
                let ident = parser.parse_identifier();
                let range = ident.range;
                names.push(ident);
                range
            },
        );

        ast::StmtGlobal {
            range: self.node_range(start),
            names,
        }
    }

    fn parse_nonlocal_stmt(&mut self) -> ast::StmtNonlocal {
        let start = self.node_start();
        self.bump(TokenKind::Nonlocal);

        let mut names = vec![];

        self.parse_separated(
            false,
            TokenKind::Comma,
            [TokenKind::Newline].as_slice(),
            |parser| {
                let ident = parser.parse_identifier();
                let range = ident.range;
                names.push(ident);
                range
            },
        );

        ast::StmtNonlocal {
            range: self.node_range(start),
            names,
        }
    }

    fn parse_return_stmt(&mut self) -> ast::StmtReturn {
        let start = self.node_start();
        self.bump(TokenKind::Return);

        let value = if self.at_expr() {
            let (value, _) = self.parse_exprs();
            Some(Box::new(value.expr))
        } else {
            None
        };

        ast::StmtReturn {
            range: self.node_range(start),
            value,
        }
    }

    fn parse_raise_stmt(&mut self) -> ast::StmtRaise {
        let start = self.node_start();
        self.bump(TokenKind::Raise);

        let exc = if self.at(TokenKind::Newline) {
            None
        } else {
            let (exc, _) = self.parse_exprs();

            if let Expr::Tuple(node) = &exc.expr {
                if !exc.is_parenthesized {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "unparenthesized tuple not allowed in `raise` statement".to_string(),
                        ),
                        node.range,
                    );
                }
            }

            Some(Box::new(exc.expr))
        };

        let cause = if exc.is_some() && self.eat(TokenKind::From) {
            let (cause, _) = self.parse_exprs();

            if let Expr::Tuple(node) = &cause.expr {
                if !cause.is_parenthesized {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "unparenthesized tuple not allowed in `raise from` statement"
                                .to_string(),
                        ),
                        node.range,
                    );
                }
            }

            Some(Box::new(cause.expr))
        } else {
            None
        };

        ast::StmtRaise {
            range: self.node_range(start),
            exc,
            cause,
        }
    }

    fn parse_type_alias_stmt(&mut self) -> ast::StmtTypeAlias {
        let start = self.node_start();
        self.bump(TokenKind::Type);

        let (tok, tok_range) = self.next_token();
        let name = if let Tok::Name { name } = tok {
            Expr::Name(ast::ExprName {
                id: name,
                ctx: ExprContext::Store,
                range: tok_range,
            })
        } else {
            self.add_error(
                ParseErrorType::OtherError(format!("expecting identifier, got {tok}")),
                tok_range,
            );
            Expr::Invalid(ast::ExprInvalid {
                value: self.src_text(tok_range).into(),
                range: tok_range,
            })
        };
        let type_params = if self.at(TokenKind::Lsqb) {
            Some(self.parse_type_params())
        } else {
            None
        };
        self.expect_and_recover(TokenKind::Equal, EXPR_SET);

        let (value, _) = self.parse_expr();

        ast::StmtTypeAlias {
            name: Box::new(name),
            type_params,
            value: Box::new(value.expr),
            range: self.node_range(start),
        }
    }

    fn parse_type_params(&mut self) -> ast::TypeParams {
        let mut type_params = vec![];
        let range = self.parse_delimited(
            true,
            TokenKind::Lsqb,
            TokenKind::Comma,
            TokenKind::Rsqb,
            |parser| {
                type_params.push(parser.parse_type_param());
            },
        );

        ast::TypeParams { range, type_params }
    }

    fn parse_type_param(&mut self) -> ast::TypeParam {
        let mut range = self.current_range();
        if self.eat(TokenKind::Star) {
            let name = self.parse_identifier();
            ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple {
                range: range.cover(name.range),
                name,
            })
        } else if self.eat(TokenKind::DoubleStar) {
            let name = self.parse_identifier();
            ast::TypeParam::ParamSpec(ast::TypeParamParamSpec {
                range: range.cover(name.range),
                name,
            })
        } else {
            let name = self.parse_identifier();
            let bound = if self.eat(TokenKind::Colon) {
                let (bound, bound_range) = self.parse_expr();
                range = range.cover(bound_range);
                Some(Box::new(bound.expr))
            } else {
                None
            };
            ast::TypeParam::TypeVar(ast::TypeParamTypeVar { range, name, bound })
        }
    }

    fn parse_dotted_name(&mut self) -> ast::Identifier {
        let id = self.parse_identifier();
        let mut range = id.range;

        while self.eat(TokenKind::Dot) {
            let id = self.parse_identifier();
            if !id.is_valid() {
                self.add_error(
                    ParseErrorType::OtherError("invalid identifier".into()),
                    id.range,
                );
            }
            range = range.cover(id.range);
        }

        ast::Identifier {
            id: self.src_text(range).into(),
            range,
        }
    }

    fn parse_alias(&mut self) -> ast::Alias {
        let mut range = self.current_range();
        if self.eat(TokenKind::Star) {
            return ast::Alias {
                name: ast::Identifier {
                    id: "*".into(),
                    range,
                },
                asname: None,
                range,
            };
        }

        let name = self.parse_dotted_name();
        range = range.cover(name.range);

        let asname = if self.eat(TokenKind::As) {
            let id = self.parse_identifier();
            range = range.cover(id.range);
            Some(id)
        } else {
            None
        };

        ast::Alias {
            range,
            name,
            asname,
        }
    }

    fn parse_import_stmt(&mut self) -> ast::StmtImport {
        let start = self.node_start();
        self.bump(TokenKind::Import);

        let mut names = vec![];
        self.parse_separated(
            false,
            TokenKind::Comma,
            [TokenKind::Newline].as_slice(),
            |parser| {
                let alias = parser.parse_alias();
                let range = alias.range;
                names.push(alias);
                range
            },
        );

        ast::StmtImport {
            range: self.node_range(start),
            names,
        }
    }

    fn parse_import_from_stmt(&mut self) -> ast::StmtImportFrom {
        let start = self.node_start();
        self.bump(TokenKind::From);

        let mut module = None;
        let mut level = if self.eat(TokenKind::Ellipsis) { 3 } else { 0 };

        const DOT_ELLIPSIS_SET: TokenSet = TokenSet::new(&[TokenKind::Dot, TokenKind::Ellipsis]);
        while self.at_ts(DOT_ELLIPSIS_SET) {
            if self.eat(TokenKind::Dot) {
                level += 1;
            }

            if self.eat(TokenKind::Ellipsis) {
                level += 3;
            }
        }

        if self.at(TokenKind::Name) {
            module = Some(self.parse_dotted_name());
        };

        if level == 0 && module.is_none() {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError("missing module name".to_string()),
                range,
            );
        }

        self.expect_and_recover(TokenKind::Import, TokenSet::EMPTY);

        let mut names = vec![];
        if self.at(TokenKind::Lpar) {
            self.parse_delimited(
                true,
                TokenKind::Lpar,
                TokenKind::Comma,
                TokenKind::Rpar,
                |parser| {
                    names.push(parser.parse_alias());
                },
            );
        } else {
            self.parse_separated(
                false,
                TokenKind::Comma,
                [TokenKind::Newline].as_slice(),
                |parser| {
                    let alias = parser.parse_alias();
                    let range = alias.range;
                    names.push(alias);
                    range
                },
            );
        };

        ast::StmtImportFrom {
            module,
            names,
            level: Some(level),
            range: self.node_range(start),
        }
    }

    const ELSE_ELIF_SET: TokenSet = TokenSet::new(&[TokenKind::Else, TokenKind::Elif]);
    fn parse_if_stmt(&mut self) -> ast::StmtIf {
        let if_start = self.node_start();
        self.bump(TokenKind::If);

        let (test, _) = self.parse_expr_with_recovery(
            Parser::parse_expr2,
            [TokenKind::Colon].as_slice(),
            "expecting expression after `if` keyword",
        );
        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        let body = self.parse_body(Clause::If);

        let elif_else_clauses = if self.at_ts(Self::ELSE_ELIF_SET) {
            self.parse_elif_else_clauses()
        } else {
            vec![]
        };

        ast::StmtIf {
            test: Box::new(test.expr),
            body,
            elif_else_clauses,
            range: self.node_range(if_start),
        }
    }

    fn parse_elif_else_clauses(&mut self) -> Vec<ast::ElifElseClause> {
        let mut elif_else_stmts = vec![];

        while self.at(TokenKind::Elif) {
            let elif_start = self.node_start();
            self.bump(TokenKind::Elif);

            let (test, _) = self.parse_expr_with_recovery(
                Parser::parse_expr2,
                [TokenKind::Colon].as_slice(),
                "expecting expression after `elif` keyword",
            );
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            let body = self.parse_body(Clause::ElIf);

            elif_else_stmts.push(ast::ElifElseClause {
                test: Some(test.expr),
                body,
                range: self.node_range(elif_start),
            });
        }

        if self.at(TokenKind::Else) {
            let else_start = self.node_start();
            self.bump(TokenKind::Else);
            self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

            let body = self.parse_body(Clause::Else);

            elif_else_stmts.push(ast::ElifElseClause {
                test: None,
                body,
                range: self.node_range(else_start),
            });
        }

        elif_else_stmts
    }

    fn parse_body(&mut self, parent_clause: Clause) -> Vec<Stmt> {
        let mut stmts = vec![];

        // Check if we are currently at a simple statement
        if !self.eat(TokenKind::Newline) && self.at_simple_stmt() {
            return self.parse_simple_stmts();
        }

        if self.eat(TokenKind::Indent) {
            const BODY_END_SET: TokenSet =
                TokenSet::new(&[TokenKind::Dedent]).union(NEWLINE_EOF_SET);
            while !self.at_ts(BODY_END_SET) {
                if self.at(TokenKind::Indent) {
                    self.handle_unexpected_indentation(
                        &mut stmts,
                        "indentation doesn't match previous indentation",
                    );
                    continue;
                }

                stmts.push(self.parse_statement());
            }

            self.eat(TokenKind::Dedent);
        } else {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "expected an indented block after {parent_clause}"
                )),
                range,
            );
        }

        stmts
    }

    /// Parses every Python expression.
    fn parse_exprs(&mut self) -> (ParsedExpr, TextRange) {
        let (parsed_expr, expr_range) = self.parse_expr();

        if self.at(TokenKind::Comma) {
            return self.parse_tuple_expr(parsed_expr.expr, expr_range, Parser::parse_expr);
        }

        (parsed_expr, expr_range)
    }

    /// Parses every Python expression except unparenthesized tuple and named expressions.
    ///
    /// NOTE: If you have expressions separated by commas and want to parse them individually,
    /// instead of a tuple, use this function!
    fn parse_expr(&mut self) -> ExprWithRange {
        let (parsed_expr, expr_range) = self.parse_expr_simple();

        if self.at(TokenKind::If) {
            return self.parse_if_expr(parsed_expr.expr, expr_range);
        }

        (parsed_expr, expr_range)
    }

    /// Parses every Python expression except unparenthesized tuple.
    ///
    /// NOTE: If you have expressions separated by commas and want to parse them individually,
    /// instead of a tuple, use this function!
    fn parse_expr2(&mut self) -> ExprWithRange {
        let (parsed_expr, expr_range) = self.parse_expr();

        if self.at(TokenKind::ColonEqual) {
            return self.parse_named_expr(parsed_expr.expr, expr_range);
        }

        (parsed_expr, expr_range)
    }

    /// Parses every Python expression except unparenthesized tuple and `if` expression.
    fn parse_expr_simple(&mut self) -> ExprWithRange {
        self.expr_bp(1)
    }

    /// Tries to parse an expression (using `parse_func`), and recovers from
    /// errors by skipping until a specified set of tokens.
    ///
    /// If the current token is not part of an expression, adds the `error_msg`
    /// to the list of errors and returns an `Expr::Invalid`.
    fn parse_expr_with_recovery(
        &mut self,
        mut parse_func: impl FnMut(&mut Parser<'src>) -> ExprWithRange,
        recover_set: impl Into<TokenSet>,
        error_msg: impl Display,
    ) -> ExprWithRange {
        if self.at_expr() {
            parse_func(self)
        } else {
            let range = self.current_range();
            self.add_error(ParseErrorType::OtherError(error_msg.to_string()), range);
            self.skip_until(NEWLINE_EOF_SET.union(recover_set.into()));

            (
                Expr::Invalid(ast::ExprInvalid {
                    value: self.src_text(range).into(),
                    range,
                })
                .into(),
                range,
            )
        }
    }

    /// Binding powers of operators for a Pratt parser.
    ///
    /// See <https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html>
    fn current_op(&mut self) -> (u8, TokenKind, Associativity) {
        const NOT_AN_OP: (u8, TokenKind, Associativity) =
            (0, TokenKind::Unknown, Associativity::Left);
        let kind = self.current_kind();

        match kind {
            TokenKind::Or => (4, kind, Associativity::Left),
            TokenKind::And => (5, kind, Associativity::Left),
            TokenKind::Not if self.peek_nth(1).0 == TokenKind::In => (7, kind, Associativity::Left),
            TokenKind::Is
            | TokenKind::In
            | TokenKind::EqEqual
            | TokenKind::NotEqual
            | TokenKind::Less
            | TokenKind::LessEqual
            | TokenKind::Greater
            | TokenKind::GreaterEqual => (7, kind, Associativity::Left),
            TokenKind::Vbar => (8, kind, Associativity::Left),
            TokenKind::CircumFlex => (9, kind, Associativity::Left),
            TokenKind::Amper => (10, kind, Associativity::Left),
            TokenKind::LeftShift | TokenKind::RightShift => (11, kind, Associativity::Left),
            TokenKind::Plus | TokenKind::Minus => (12, kind, Associativity::Left),
            TokenKind::Star
            | TokenKind::Slash
            | TokenKind::DoubleSlash
            | TokenKind::Percent
            | TokenKind::At => (14, kind, Associativity::Left),
            TokenKind::DoubleStar => (18, kind, Associativity::Right),
            _ => NOT_AN_OP,
        }
    }

    /// Parses expression with binding power of at least bp.
    ///
    /// Uses the Pratt parser algorithm.
    /// See <https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html>
    fn expr_bp(&mut self, bp: u8) -> ExprWithRange {
        let (mut lhs, mut lhs_range) = self.parse_lhs();

        loop {
            let (op_bp, op, associativity) = self.current_op();
            if op_bp < bp {
                break;
            }

            // Don't parse a `CompareExpr` if we are parsing a `Comprehension` or `ForStmt`
            if op.is_compare_operator() && self.has_ctx(ParserCtxFlags::FOR_TARGET) {
                break;
            }

            let op_bp = match associativity {
                Associativity::Left => op_bp + 1,
                Associativity::Right => op_bp,
            };

            self.eat(op);

            // We need to create a dedicated node for boolean operations,
            // even though boolean operations are infix.
            if op.is_bool_operator() {
                (lhs, lhs_range) = self.parse_bool_op_expr(lhs.expr, lhs_range, op, op_bp);
                continue;
            }

            // Same here as well
            if op.is_compare_operator() {
                (lhs, lhs_range) = self.parse_compare_op_expr(lhs.expr, lhs_range, op, op_bp);
                continue;
            }

            let (rhs, rhs_range) = if self.at_expr() {
                self.expr_bp(op_bp)
            } else {
                let rhs_range = self.current_range();
                self.add_error(
                    ParseErrorType::OtherError("expecting an expression after operand".into()),
                    rhs_range,
                );
                (
                    Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(rhs_range).into(),
                        range: rhs_range,
                    })
                    .into(),
                    rhs_range,
                )
            };
            lhs_range = lhs_range.cover(rhs_range);
            lhs.expr = Expr::BinOp(ast::ExprBinOp {
                left: Box::new(lhs.expr),
                op: Operator::try_from(op).unwrap(),
                right: Box::new(rhs.expr),
                range: lhs_range,
            });
        }

        (lhs, lhs_range)
    }

    fn parse_lhs(&mut self) -> ExprWithRange {
        let token = self.next_token();
        let (mut lhs, mut lhs_range) = match token.0 {
            Tok::Plus | Tok::Minus | Tok::Not | Tok::Tilde => self.parse_unary_expr(token),
            Tok::Star => self.parse_starred_expr(token),
            Tok::Await => self.parse_await_expr(token.1),
            Tok::Lambda => self.parse_lambda_expr(token.1),
            _ => self.parse_atom(token),
        };

        if self.is_current_token_postfix() {
            (lhs, lhs_range) = self.parse_postfix_expr(lhs.expr, lhs_range);
        }

        (lhs, lhs_range)
    }

    fn parse_identifier(&mut self) -> ast::Identifier {
        let range = self.current_range();
        if self.current_kind() == TokenKind::Name {
            let (Tok::Name { name }, _) = self.next_token() else {
                unreachable!();
            };
            ast::Identifier { id: name, range }
        } else {
            self.add_error(
                ParseErrorType::OtherError("expecting an identifier".into()),
                range,
            );
            ast::Identifier {
                id: String::new(),
                range,
            }
        }
    }

    fn parse_atom(&mut self, token: Spanned) -> ExprWithRange {
        let (tok, mut range) = token;
        let lhs = match tok {
            Tok::Float { value } => Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: Number::Float(value),
                range,
            }),
            Tok::Complex { real, imag } => Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: Number::Complex { real, imag },
                range,
            }),
            Tok::Int { value } => Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: Number::Int(value),
                range,
            }),
            Tok::True => Expr::BooleanLiteral(ast::ExprBooleanLiteral { value: true, range }),
            Tok::False => Expr::BooleanLiteral(ast::ExprBooleanLiteral {
                value: false,
                range,
            }),
            Tok::None => Expr::NoneLiteral(ast::ExprNoneLiteral { range }),
            Tok::Ellipsis => Expr::EllipsisLiteral(ast::ExprEllipsisLiteral { range }),
            Tok::Name { name } => Expr::Name(ast::ExprName {
                id: name,
                ctx: ExprContext::Load,
                range,
            }),
            Tok::IpyEscapeCommand { value, kind } if self.mode == Mode::Ipython => {
                Expr::IpyEscapeCommand(ast::ExprIpyEscapeCommand { range, kind, value })
            }
            tok @ Tok::String { .. } => return self.parse_string_expr(tok, range),
            Tok::FStringStart => return self.parse_fstring_expr(range),
            Tok::Lpar => return self.parse_parenthesized_expr(range),
            Tok::Lsqb => return self.parse_bracketsized_expr(range),
            Tok::Lbrace => return self.parse_bracesized_expr(range),
            Tok::Yield => return self.parse_yield_expr(range),
            // `Invalid` tokens are created when there's a lexical error, to
            // avoid creating an "unexpected token" error for `Tok::Invalid`
            // we handle it here. We try to parse an expression to avoid
            // creating "statements in the same line" error in some cases.
            Tok::Unknown => {
                if self.at_expr() {
                    let (parsed_expr, expr_range) = self.parse_exprs();
                    range = expr_range;
                    parsed_expr.expr
                } else {
                    Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(range).into(),
                        range,
                    })
                }
            }
            // Handle unexpected token
            tok => {
                // Try to parse an expression after seeing an unexpected token
                let lhs = if self.at_expr() {
                    let (parsed_expr, expr_range) = self.parse_exprs();
                    range = expr_range;
                    parsed_expr.expr
                } else {
                    Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(range).into(),
                        range,
                    })
                };

                if matches!(tok, Tok::IpyEscapeCommand { .. }) {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "IPython escape commands are only allowed in `Mode::Ipython`".into(),
                        ),
                        range,
                    );
                } else {
                    self.add_error(
                        ParseErrorType::OtherError(format!("unexpected token `{tok}`")),
                        range,
                    );
                }
                lhs
            }
        };

        (lhs.into(), range)
    }

    fn parse_postfix_expr(&mut self, mut lhs: Expr, mut lhs_range: TextRange) -> ExprWithRange {
        loop {
            let (parsed_expr, range) = match self.current_kind() {
                TokenKind::Lpar => self.parse_call_expr(lhs, lhs_range),
                TokenKind::Lsqb => self.parse_subscript_expr(lhs, lhs_range),
                TokenKind::Dot => self.parse_attribute_expr(lhs, lhs_range),
                _ => break,
            };
            lhs = parsed_expr.expr;
            lhs_range = range;
        }

        (lhs.into(), lhs_range)
    }

    fn parse_call_expr(&mut self, lhs: Expr, lhs_range: TextRange) -> ExprWithRange {
        assert!(self.at(TokenKind::Lpar));
        let arguments = self.parse_arguments();
        let range = lhs_range.cover(arguments.range);

        (
            Expr::Call(ast::ExprCall {
                func: Box::new(lhs),
                arguments,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_arguments(&mut self) -> ast::Arguments {
        self.set_ctx(ParserCtxFlags::ARGUMENTS);

        let mut args: Vec<Expr> = vec![];
        let mut keywords: Vec<ast::Keyword> = vec![];
        let mut has_seen_kw_arg = false;
        let mut has_seen_kw_unpack = false;

        let range = self.parse_delimited(
            true,
            TokenKind::Lpar,
            TokenKind::Comma,
            TokenKind::Rpar,
            |parser| {
                if parser.at(TokenKind::DoubleStar) {
                    let range = parser.current_range();
                    parser.eat(TokenKind::DoubleStar);

                    let (value, value_range) = parser.parse_expr();
                    keywords.push(ast::Keyword {
                        arg: None,
                        value: value.expr,
                        range: range.cover(value_range),
                    });

                    has_seen_kw_unpack = true;
                } else {
                    let (mut parsed_expr, expr_range) = parser.parse_expr2();

                    match parser.current_kind() {
                        TokenKind::Async | TokenKind::For => {
                            (parsed_expr, _) =
                                parser.parse_generator_expr(parsed_expr.expr, expr_range);
                        }
                        _ => {}
                    }

                    if has_seen_kw_unpack && matches!(parsed_expr.expr, Expr::Starred(_)) {
                        parser.add_error(ParseErrorType::UnpackedArgumentError, expr_range);
                    }

                    if parser.eat(TokenKind::Equal) {
                        has_seen_kw_arg = true;
                        let arg = if let Expr::Name(ident_expr) = parsed_expr.expr {
                            ast::Identifier {
                                id: ident_expr.id,
                                range: ident_expr.range,
                            }
                        } else {
                            parser.add_error(
                                ParseErrorType::OtherError(format!(
                                    "`{}` cannot be used as a keyword argument!",
                                    parser.src_text(expr_range)
                                )),
                                expr_range,
                            );
                            ast::Identifier {
                                id: String::new(),
                                range: expr_range,
                            }
                        };

                        let (value, value_range) = parser.parse_expr();

                        keywords.push(ast::Keyword {
                            arg: Some(arg),
                            value: value.expr,
                            range: expr_range.cover(value_range),
                        });
                    } else {
                        if has_seen_kw_arg
                            && !(has_seen_kw_unpack || matches!(parsed_expr.expr, Expr::Starred(_)))
                        {
                            parser.add_error(ParseErrorType::PositionalArgumentError, expr_range);
                        }
                        args.push(parsed_expr.expr);
                    }
                }
            },
        );
        self.clear_ctx(ParserCtxFlags::ARGUMENTS);

        let arguments = ast::Arguments {
            range,
            args,
            keywords,
        };

        if let Err(error) = helpers::validate_arguments(&arguments) {
            self.errors.push(error);
        }

        arguments
    }

    fn parse_subscript_expr(&mut self, mut value: Expr, value_range: TextRange) -> ExprWithRange {
        assert!(self.eat(TokenKind::Lsqb));

        // To prevent the `value` context from being `Del` within a `del` statement,
        // we set the context as `Load` here.
        helpers::set_expr_ctx(&mut value, ExprContext::Load);

        // Create an error when receiving a empty slice to parse, e.g. `l[]`
        if !self.at(TokenKind::Colon) && !self.at_expr() {
            let close_bracket_range = self.current_range();
            self.expect_and_recover(TokenKind::Rsqb, TokenSet::EMPTY);

            let range = value_range.cover(close_bracket_range);
            let slice_range = close_bracket_range.sub_start(1.into());
            self.add_error(ParseErrorType::EmptySlice, range);
            return (
                Expr::Subscript(ast::ExprSubscript {
                    value: Box::new(value),
                    slice: Box::new(Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(slice_range).into(),
                        range: slice_range,
                    })),
                    ctx: ExprContext::Load,
                    range,
                })
                .into(),
                range,
            );
        }

        let (mut slice, slice_range) = self.parse_slice();

        if self.at(TokenKind::Comma) {
            let (_, comma_range) = self.next_token();
            let mut slices = vec![slice.expr];
            let slices_range = self
                .parse_separated(
                    true,
                    TokenKind::Comma,
                    TokenSet::new(&[TokenKind::Rsqb]),
                    |parser| {
                        let (slice, slice_range) = parser.parse_slice();
                        slices.push(slice.expr);
                        slice_range
                    },
                )
                .unwrap_or(comma_range);

            slice.expr = Expr::Tuple(ast::ExprTuple {
                elts: slices,
                ctx: ExprContext::Load,
                range: slice_range.cover(slices_range),
            });
        }

        let end_range = self.current_range();
        self.expect_and_recover(TokenKind::Rsqb, TokenSet::EMPTY);

        let range = value_range.cover(end_range);
        (
            Expr::Subscript(ast::ExprSubscript {
                value: Box::new(value),
                slice: Box::new(slice.expr),
                ctx: ExprContext::Load,
                range,
            })
            .into(),
            range,
        )
    }

    const UPPER_END_SET: TokenSet =
        TokenSet::new(&[TokenKind::Comma, TokenKind::Colon, TokenKind::Rsqb])
            .union(NEWLINE_EOF_SET);
    const STEP_END_SET: TokenSet =
        TokenSet::new(&[TokenKind::Comma, TokenKind::Rsqb]).union(NEWLINE_EOF_SET);
    fn parse_slice(&mut self) -> ExprWithRange {
        let mut range = self.current_range();
        let lower = if self.at_expr() {
            let (parsed_expr, expr_range) = self.parse_expr2();
            range = range.cover(expr_range);
            Some(parsed_expr.expr)
        } else {
            None
        };

        if self.at(TokenKind::Colon)
            && (lower.is_none()
                || lower
                    .as_ref()
                    .is_some_and(|parsed_expr| !matches!(parsed_expr, Expr::NamedExpr(_))))
        {
            let (_, colon_range) = self.next_token();
            range = range.cover(colon_range);
            let lower = lower.map(Box::new);
            let upper = if self.at_ts(Self::UPPER_END_SET) {
                None
            } else {
                let (upper, upper_range) = self.parse_expr();
                range = range.cover(upper_range);
                Some(Box::new(upper.expr))
            };

            let colon_range = self.current_range();
            let step = if self.eat(TokenKind::Colon) {
                range = range.cover(colon_range);
                if self.at_ts(Self::STEP_END_SET) {
                    None
                } else {
                    let (step, step_range) = self.parse_expr();
                    range = range.cover(step_range);
                    Some(Box::new(step.expr))
                }
            } else {
                None
            };

            (
                Expr::Slice(ast::ExprSlice {
                    range,
                    lower,
                    upper,
                    step,
                })
                .into(),
                range,
            )
        } else {
            (lower.unwrap().into(), range)
        }
    }

    fn parse_unary_expr(&mut self, (op_tok, range): Spanned) -> ExprWithRange {
        let (rhs, rhs_range) = if matches!(op_tok, Tok::Not) {
            self.expr_bp(6)
        } else {
            // plus, minus and tilde
            self.expr_bp(17)
        };
        let new_range = range.cover(rhs_range);

        (
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: UnaryOp::try_from(op_tok).unwrap(),
                operand: Box::new(rhs.expr),
                range: new_range,
            })
            .into(),
            new_range,
        )
    }

    fn parse_attribute_expr(&mut self, value: Expr, lhs_range: TextRange) -> ExprWithRange {
        assert!(self.eat(TokenKind::Dot));

        let attr = self.parse_identifier();
        let range = lhs_range.cover(attr.range);

        (
            Expr::Attribute(ast::ExprAttribute {
                value: Box::new(value),
                attr,
                ctx: ExprContext::Load,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_bool_op_expr(
        &mut self,
        lhs: Expr,
        mut lhs_range: TextRange,
        op: TokenKind,
        op_bp: u8,
    ) -> ExprWithRange {
        let mut values = vec![lhs];

        // Keep adding `expr` to `values` until we see a different
        // boolean operation than `op`.
        loop {
            let (parsed_expr, expr_range) = self.expr_bp(op_bp);
            lhs_range = lhs_range.cover(expr_range);
            values.push(parsed_expr.expr);

            if self.current_kind() != op {
                break;
            }

            self.next_token();
        }

        (
            Expr::BoolOp(ast::ExprBoolOp {
                values,
                op: BoolOp::try_from(op).unwrap(),
                range: lhs_range,
            })
            .into(),
            lhs_range,
        )
    }

    fn parse_compare_op_expr(
        &mut self,
        lhs: Expr,
        mut lhs_range: TextRange,
        op: TokenKind,
        op_bp: u8,
    ) -> ExprWithRange {
        let mut comparators = vec![];
        let op = token_kind_to_cmp_op([op, self.current_kind()]).unwrap();
        let mut ops = vec![op];

        if matches!(op, CmpOp::IsNot | CmpOp::NotIn) {
            self.next_token();
        }

        loop {
            let (parsed_expr, expr_range) = self.expr_bp(op_bp);
            lhs_range = lhs_range.cover(expr_range);
            comparators.push(parsed_expr.expr);

            if let Ok(op) = token_kind_to_cmp_op([self.current_kind(), self.peek_nth(1).0]) {
                if matches!(op, CmpOp::IsNot | CmpOp::NotIn) {
                    self.next_token();
                }

                ops.push(op);
            } else {
                break;
            }

            self.next_token();
        }

        (
            Expr::Compare(ast::ExprCompare {
                left: Box::new(lhs),
                ops,
                comparators,
                range: lhs_range,
            })
            .into(),
            lhs_range,
        )
    }

    fn parse_string_expr(&mut self, mut tok: Tok, mut str_range: TextRange) -> ExprWithRange {
        let mut final_range = str_range;
        let mut strings = vec![];
        while let Tok::String {
            value,
            kind,
            triple_quoted,
        } = tok
        {
            match parse_string_literal(&value, kind, triple_quoted, str_range) {
                Ok(string) => {
                    strings.push(string);
                }
                Err(error) => {
                    strings.push(StringType::Invalid(ast::StringLiteral {
                        value,
                        range: str_range,
                        unicode: kind.is_unicode(),
                    }));
                    self.add_error(ParseErrorType::Lexical(error.error), error.location);
                }
            }

            if !self.at(TokenKind::String) {
                break;
            }

            (tok, str_range) = self.next_token();
            final_range = final_range.cover(str_range);
        }

        // This handles the case where the string is implicit concatenated with
        // a fstring, e.g., `"hello " f"{x}"`.
        if self.at(TokenKind::FStringStart) {
            let mut fstring_range = self.current_range();
            self.handle_implicit_concatenated_strings(&mut fstring_range, &mut strings);
            final_range = final_range.cover(fstring_range);
        }

        if strings.len() == 1 {
            return match strings.pop().unwrap() {
                StringType::Str(string) => {
                    let range = string.range;
                    (
                        Expr::StringLiteral(ast::ExprStringLiteral {
                            value: ast::StringLiteralValue::single(string),
                            range,
                        })
                        .into(),
                        range,
                    )
                }
                StringType::Bytes(bytes) => {
                    let range = bytes.range;
                    (
                        Expr::BytesLiteral(ast::ExprBytesLiteral {
                            value: ast::BytesLiteralValue::single(bytes),
                            range,
                        })
                        .into(),
                        range,
                    )
                }
                StringType::Invalid(invalid) => (
                    Expr::Invalid(ast::ExprInvalid {
                        value: invalid.value,
                        range: invalid.range,
                    })
                    .into(),
                    invalid.range,
                ),
                StringType::FString(_) => unreachable!(),
            };
        }

        match concatenated_strings(strings, final_range) {
            Ok(string) => (string.into(), final_range),
            Err(error) => {
                self.add_error(ParseErrorType::Lexical(error.error), error.location);
                (
                    Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(error.location).into(),
                        range: error.location,
                    })
                    .into(),
                    error.location,
                )
            }
        }
    }

    const FSTRING_SET: TokenSet = TokenSet::new(&[TokenKind::FStringStart, TokenKind::String]);
    /// Handles implicit concatenated f-strings, e.g. `f"{x}" f"hello"`, and
    /// implicit concatenated f-strings with strings, e.g. `f"{x}" "xyz" f"{x}"`.
    fn handle_implicit_concatenated_strings(
        &mut self,
        fstring_range: &mut TextRange,
        strings: &mut Vec<StringType>,
    ) {
        while self.at_ts(Self::FSTRING_SET) {
            if self.at(TokenKind::FStringStart) {
                let (_, range) = self.next_token();
                let fstring = self.parse_fstring(range);
                *fstring_range = fstring_range.cover(fstring.range);
                strings.push(StringType::FString(fstring));
            } else {
                let (
                    Tok::String {
                        value,
                        kind,
                        triple_quoted,
                    },
                    str_range,
                ) = self.next_token()
                else {
                    unreachable!()
                };

                match parse_string_literal(&value, kind, triple_quoted, str_range) {
                    Ok(string) => {
                        *fstring_range = fstring_range.cover(str_range);
                        strings.push(string);
                    }
                    Err(error) => {
                        strings.push(StringType::Invalid(ast::StringLiteral {
                            value,
                            range: str_range,
                            unicode: kind.is_unicode(),
                        }));
                        self.add_error(ParseErrorType::Lexical(error.error), error.location);
                    }
                }
            }
        }
    }

    fn parse_fstring_expr(&mut self, mut fstring_range: TextRange) -> ExprWithRange {
        let fstring = self.parse_fstring(fstring_range);

        if !self.at_ts(Self::FSTRING_SET) {
            let range = fstring.range;
            return (
                Expr::FString(ast::ExprFString {
                    value: ast::FStringValue::single(fstring),
                    range,
                })
                .into(),
                range,
            );
        }

        let mut strings = vec![StringType::FString(fstring)];
        self.handle_implicit_concatenated_strings(&mut fstring_range, &mut strings);

        match concatenated_strings(strings, fstring_range) {
            Ok(string) => (string.into(), fstring_range),
            Err(error) => {
                self.add_error(ParseErrorType::Lexical(error.error), error.location);
                (
                    Expr::Invalid(ast::ExprInvalid {
                        value: self.src_text(error.location).into(),
                        range: error.location,
                    })
                    .into(),
                    error.location,
                )
            }
        }
    }

    fn parse_fstring(&mut self, mut fstring_range: TextRange) -> ast::FString {
        let (elements, _) = self.parse_fstring_elements();

        fstring_range = fstring_range.cover(self.current_range());
        self.eat(TokenKind::FStringEnd);

        ast::FString {
            elements,
            range: fstring_range,
        }
    }

    const FSTRING_END_SET: TokenSet =
        TokenSet::new(&[TokenKind::FStringEnd, TokenKind::Rbrace]).union(NEWLINE_EOF_SET);
    fn parse_fstring_elements(&mut self) -> (Vec<FStringElement>, TextRange) {
        let mut elements = vec![];
        let mut final_range: Option<TextRange> = None;
        while !self.at_ts(Self::FSTRING_END_SET) {
            let element = match self.current_kind() {
                TokenKind::Lbrace => {
                    let fstring_expr = self.parse_fstring_expr_element();
                    let range = final_range.get_or_insert(fstring_expr.range);
                    *range = range.cover(fstring_expr.range);
                    FStringElement::Expression(fstring_expr)
                }
                TokenKind::FStringMiddle => {
                    let (Tok::FStringMiddle { value, is_raw }, range) = self.next_token() else {
                        unreachable!()
                    };
                    let (fstring_literal, fstring_range) =
                        match parse_fstring_literal_element(&value, is_raw, range) {
                            Ok(fstring) => {
                                let range = fstring.range();
                                (fstring, range)
                            }
                            Err(lex_error) => {
                                self.add_error(
                                    ParseErrorType::Lexical(lex_error.error),
                                    lex_error.location,
                                );
                                (
                                    ast::FStringElement::Invalid(ast::FStringInvalidElement {
                                        value: self.src_text(lex_error.location).into(),
                                        range: lex_error.location,
                                    }),
                                    lex_error.location,
                                )
                            }
                        };
                    let range = final_range.get_or_insert(fstring_range);
                    *range = range.cover(fstring_range);
                    fstring_literal
                }
                // `Invalid` tokens are created when there's a lexical error, so
                // we ignore it here to avoid creating unexpected token errors
                TokenKind::Unknown => {
                    self.next_token();
                    continue;
                }
                // Handle an unexpected token
                _ => {
                    let (tok, range) = self.next_token();
                    self.add_error(
                        ParseErrorType::OtherError(format!("f-string: unexpected token `{tok:?}`")),
                        range,
                    );
                    continue;
                }
            };
            elements.push(element);
        }

        (elements, final_range.unwrap_or_default())
    }

    fn parse_fstring_expr_element(&mut self) -> ast::FStringExpressionElement {
        let range = self.current_range();

        let has_open_brace = self.eat(TokenKind::Lbrace);
        let (value, value_range) = self.parse_expr_with_recovery(
            Parser::parse_exprs,
            [
                TokenKind::Exclamation,
                TokenKind::Colon,
                TokenKind::Rbrace,
                TokenKind::FStringEnd,
            ]
            .as_slice(),
            "f-string: expecting expression",
        );
        if !value.is_parenthesized && matches!(value.expr, Expr::Lambda(_)) {
            self.add_error(
                ParseErrorType::FStringError(FStringErrorType::LambdaWithoutParentheses),
                value_range,
            );
        }
        let debug_text = if self.eat(TokenKind::Equal) {
            let leading_range = range
                .add_start("{".text_len())
                .cover_offset(value_range.start());
            let trailing_range = TextRange::new(value_range.end(), self.current_range().start());
            Some(ast::DebugText {
                leading: self.src_text(leading_range).to_string(),
                trailing: self.src_text(trailing_range).to_string(),
            })
        } else {
            None
        };

        let conversion = if self.eat(TokenKind::Exclamation) {
            let (_, range) = self.next_token();
            match self.src_text(range) {
                "s" => ConversionFlag::Str,
                "r" => ConversionFlag::Repr,
                "a" => ConversionFlag::Ascii,
                _ => {
                    self.add_error(
                        ParseErrorType::FStringError(FStringErrorType::InvalidConversionFlag),
                        range,
                    );
                    ConversionFlag::None
                }
            }
        } else {
            ConversionFlag::None
        };

        let format_spec = if self.eat(TokenKind::Colon) {
            let (elements, mut range) = self.parse_fstring_elements();
            // Special case for when the f-string format spec is empty. We set the range
            // to an empty `TextRange`.
            if range.is_empty() {
                range = TextRange::empty(self.current_range().start());
            }
            Some(Box::new(ast::FStringFormatSpec { range, elements }))
        } else {
            None
        };

        let close_brace_range = self.current_range();
        if has_open_brace && !self.eat(TokenKind::Rbrace) {
            self.add_error(
                ParseErrorType::FStringError(FStringErrorType::UnclosedLbrace),
                close_brace_range,
            );
        }

        ast::FStringExpressionElement {
            expression: Box::new(value.expr),
            debug_text,
            conversion,
            format_spec,
            range: range.cover(close_brace_range),
        }
    }

    fn parse_bracketsized_expr(&mut self, open_bracket_range: TextRange) -> ExprWithRange {
        // Nice error message when having a unclosed open bracket `[`
        if self.at_ts(NEWLINE_EOF_SET) {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError("missing closing bracket `]`".to_string()),
                range,
            );
        }

        // Return an empty `ListExpr` when finding a `]` right after the `[`
        if self.at(TokenKind::Rsqb) {
            let close_bracket_range = self.current_range();
            let range = open_bracket_range.cover(close_bracket_range);

            self.eat(TokenKind::Rsqb);
            return (
                Expr::List(ast::ExprList {
                    elts: vec![],
                    ctx: ExprContext::Load,
                    range,
                })
                .into(),
                range,
            );
        }

        let (mut parsed_expr, expr_range) = self.parse_expr2();

        match self.current_kind() {
            TokenKind::Async | TokenKind::For => {
                (parsed_expr, _) = self.parse_list_comprehension_expr(parsed_expr.expr, expr_range);
            }
            _ => {
                (parsed_expr, _) = self.parse_list_expr(parsed_expr.expr);
            }
        }
        let close_bracket_range = self.current_range();
        self.expect_and_recover(TokenKind::Rsqb, TokenSet::EMPTY);

        let range = open_bracket_range.cover(close_bracket_range);

        // Update the range of `Expr::List` or `Expr::ListComp` to
        // include the parenthesis.
        if matches!(parsed_expr.expr, Expr::List(_) | Expr::ListComp(_)) {
            helpers::set_expr_range(&mut parsed_expr.expr, range);
        }

        (parsed_expr, range)
    }

    fn parse_bracesized_expr(&mut self, lbrace_range: TextRange) -> ExprWithRange {
        // Nice error message when having a unclosed open brace `{`
        if self.at_ts(NEWLINE_EOF_SET) {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError("missing closing brace `}`".to_string()),
                range,
            );
        }

        // Return an empty `DictExpr` when finding a `}` right after the `{`
        if self.at(TokenKind::Rbrace) {
            let close_brace_range = self.current_range();
            let range = lbrace_range.cover(close_brace_range);

            self.eat(TokenKind::Rbrace);
            return (
                Expr::Dict(ast::ExprDict {
                    keys: vec![],
                    values: vec![],
                    range,
                })
                .into(),
                range,
            );
        }

        let (mut parsed_expr, mut expr_range) = if self.eat(TokenKind::DoubleStar) {
            // Handle dict unpack
            let (value, _) = self.parse_expr();
            self.parse_dict_expr(None, value.expr)
        } else {
            self.parse_expr2()
        };

        match self.current_kind() {
            TokenKind::Async | TokenKind::For => {
                (parsed_expr, expr_range) =
                    self.parse_set_comprehension_expr(parsed_expr.expr, expr_range);
            }
            TokenKind::Colon => {
                self.next_token();
                let (value, value_range) = self.parse_expr();
                let range = expr_range.cover(value_range);

                (parsed_expr, expr_range) = match self.current_kind() {
                    TokenKind::Async | TokenKind::For => {
                        self.parse_dict_comprehension_expr(parsed_expr.expr, value.expr, range)
                    }
                    _ => self.parse_dict_expr(Some(parsed_expr.expr), value.expr),
                };
            }
            _ if !matches!(parsed_expr.expr, Expr::Dict(_)) => {
                (parsed_expr, expr_range) = self.parse_set_expr(parsed_expr.expr);
            }
            _ => {}
        }

        let rbrace_range = self.current_range();
        self.expect_and_recover(TokenKind::Rbrace, TokenSet::EMPTY);

        // Check for dict unpack used in a comprehension, e.g. `{**d for i in l}`
        if matches!(
            parsed_expr.expr,
            Expr::SetComp(ast::ExprSetComp { ref elt, .. }) if matches!(elt.as_ref(), Expr::Dict(_))
        ) {
            self.add_error(
                ParseErrorType::OtherError(
                    "dict unpacking cannot be used in dict comprehension".into(),
                ),
                expr_range,
            );
        }

        let range = lbrace_range.cover(rbrace_range);
        // Update the range of `Expr::Set`, `Expr::Dict`, `Expr::DictComp` and
        // `Expr::SetComp` to include the parenthesis.
        if matches!(
            parsed_expr.expr,
            Expr::Set(_) | Expr::Dict(_) | Expr::DictComp(_) | Expr::SetComp(_)
        ) {
            helpers::set_expr_range(&mut parsed_expr.expr, range);
        }

        (parsed_expr, range)
    }

    fn parse_parenthesized_expr(&mut self, open_paren_range: TextRange) -> ExprWithRange {
        self.set_ctx(ParserCtxFlags::PARENTHESIZED_EXPR);

        // Nice error message when having a unclosed open parenthesis `(`
        if self.at_ts(NEWLINE_EOF_SET) {
            let range = self.current_range();
            self.add_error(
                ParseErrorType::OtherError("missing closing parenthesis `)`".to_string()),
                range,
            );
        }

        // Return an empty `TupleExpr` when finding a `)` right after the `(`
        if self.at(TokenKind::Rpar) {
            let close_paren_range = self.current_range();
            let range = open_paren_range.cover(close_paren_range);

            self.eat(TokenKind::Rpar);
            self.clear_ctx(ParserCtxFlags::PARENTHESIZED_EXPR);

            return (
                Expr::Tuple(ast::ExprTuple {
                    elts: vec![],
                    ctx: ExprContext::Load,
                    range,
                })
                .into(),
                range,
            );
        }

        let (mut parsed_expr, expr_range) = self.parse_expr2();

        match self.current_kind() {
            TokenKind::Comma => {
                (parsed_expr, _) =
                    self.parse_tuple_expr(parsed_expr.expr, expr_range, Parser::parse_expr2);
            }
            TokenKind::Async | TokenKind::For => {
                (parsed_expr, _) = self.parse_generator_expr(parsed_expr.expr, expr_range);
            }
            _ => {}
        }
        let close_paren_range = self.current_range();
        self.expect_and_recover(TokenKind::Rpar, TokenSet::EMPTY);

        let range = open_paren_range.cover(close_paren_range);

        // Update the range of `Expr::Tuple` or `Expr::Generator` to
        // include the parenthesis.
        if matches!(parsed_expr.expr, Expr::Tuple(_) | Expr::GeneratorExp(_))
            && !parsed_expr.is_parenthesized
        {
            helpers::set_expr_range(&mut parsed_expr.expr, range);
        }

        self.clear_ctx(ParserCtxFlags::PARENTHESIZED_EXPR);
        parsed_expr.is_parenthesized = true;

        (parsed_expr, range)
    }

    const END_SEQUENCE_SET: TokenSet = END_EXPR_SET.remove(TokenKind::Comma);
    /// Parses multiple items separated by a comma into a `TupleExpr` node.
    /// Uses `parse_func` to parse each item.
    fn parse_tuple_expr(
        &mut self,
        first_element: Expr,
        first_element_range: TextRange,
        mut parse_func: impl FnMut(&mut Parser<'src>) -> ExprWithRange,
    ) -> ExprWithRange {
        // In case of the tuple only having one element, we need to cover the
        // range of the comma.
        let mut final_range = first_element_range.cover(self.current_range());
        if !self.at_ts(Self::END_SEQUENCE_SET) {
            self.expect(TokenKind::Comma);
        }

        let mut elts = vec![first_element];

        final_range = final_range.cover(
            self.parse_separated(true, TokenKind::Comma, Self::END_SEQUENCE_SET, |parser| {
                let (parsed_expr, range) = parse_func(parser);
                elts.push(parsed_expr.expr);
                range
            })
            .unwrap_or(final_range),
        );

        (
            Expr::Tuple(ast::ExprTuple {
                elts,
                ctx: ExprContext::Load,
                range: final_range,
            })
            .into(),
            final_range,
        )
    }

    fn parse_list_expr(&mut self, first_element: Expr) -> ExprWithRange {
        if !self.at_ts(Self::END_SEQUENCE_SET) {
            self.expect(TokenKind::Comma);
        }
        let mut elts = vec![first_element];

        let range = self
            .parse_separated(true, TokenKind::Comma, Self::END_SEQUENCE_SET, |parser| {
                let (parsed_expr, range) = parser.parse_expr2();
                elts.push(parsed_expr.expr);
                range
            })
            // Doesn't really matter what range we get here, since the range will
            // be modified later in `parse_bracketsized_expr`.
            .unwrap_or_default();

        (
            Expr::List(ast::ExprList {
                elts,
                ctx: ExprContext::Load,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_set_expr(&mut self, first_element: Expr) -> ExprWithRange {
        if !self.at_ts(Self::END_SEQUENCE_SET) {
            self.expect(TokenKind::Comma);
        }
        let mut elts = vec![first_element];

        let range = self
            .parse_separated(true, TokenKind::Comma, Self::END_SEQUENCE_SET, |parser| {
                let (parsed_expr, range) = parser.parse_expr2();
                elts.push(parsed_expr.expr);
                range
            })
            // Doesn't really matter what range we get here, since the range will
            // be modified later in `parse_bracesized_expr`.
            .unwrap_or_default();

        (Expr::Set(ast::ExprSet { range, elts }).into(), range)
    }

    fn parse_dict_expr(&mut self, key: Option<Expr>, value: Expr) -> ExprWithRange {
        if !self.at_ts(Self::END_SEQUENCE_SET) {
            self.expect(TokenKind::Comma);
        }

        let mut keys = vec![key];
        let mut values = vec![value];

        let range = self
            .parse_separated(true, TokenKind::Comma, Self::END_SEQUENCE_SET, |parser| {
                if parser.eat(TokenKind::DoubleStar) {
                    keys.push(None);
                } else {
                    let (key, _) = parser.parse_expr();
                    keys.push(Some(key.expr));

                    parser.expect_and_recover(
                        TokenKind::Colon,
                        TokenSet::new(&[TokenKind::Comma]).union(EXPR_SET),
                    );
                }
                let (value, range) = parser.parse_expr();
                values.push(value.expr);
                range
            })
            // Doesn't really matter what range we get here, since the range will
            // be modified later in `parse_bracesized_expr`.
            .unwrap_or_default();

        (
            Expr::Dict(ast::ExprDict {
                range,
                keys,
                values,
            })
            .into(),
            range,
        )
    }

    fn parse_comprehension(&mut self) -> ast::Comprehension {
        assert!(self.at(TokenKind::For) || self.at(TokenKind::Async));

        let mut range = self.current_range();

        let is_async = self.eat(TokenKind::Async);
        self.eat(TokenKind::For);

        self.set_ctx(ParserCtxFlags::FOR_TARGET);
        let (mut target, _) = self.parse_expr_with_recovery(
            Parser::parse_exprs,
            [TokenKind::In, TokenKind::Colon].as_slice(),
            "expecting expression after `for` keyword",
        );
        self.clear_ctx(ParserCtxFlags::FOR_TARGET);

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        self.expect_and_recover(TokenKind::In, TokenSet::new(&[TokenKind::Rsqb]));

        let (iter, iter_expr) = self.parse_expr_with_recovery(
            Parser::parse_expr_simple,
            EXPR_SET.union(
                [
                    TokenKind::Rpar,
                    TokenKind::Rsqb,
                    TokenKind::Rbrace,
                    TokenKind::If,
                    TokenKind::Async,
                    TokenKind::For,
                ]
                .as_slice()
                .into(),
            ),
            "expecting an expression after `in` keyword",
        );
        range = range.cover(iter_expr);

        let mut ifs = vec![];
        while self.eat(TokenKind::If) {
            let (if_expr, if_range) = self.parse_expr_simple();
            ifs.push(if_expr.expr);
            range = range.cover(if_range);
        }

        ast::Comprehension {
            range,
            target: target.expr,
            iter: iter.expr,
            ifs,
            is_async,
        }
    }

    fn parse_generators(&mut self, mut range: TextRange) -> (Vec<ast::Comprehension>, TextRange) {
        const GENERATOR_SET: TokenSet = TokenSet::new(&[TokenKind::For, TokenKind::Async]);
        let mut generators = vec![];
        while self.at_ts(GENERATOR_SET) {
            let comp = self.parse_comprehension();
            range = range.cover(comp.range);

            generators.push(comp);
        }

        (generators, range)
    }

    fn parse_generator_expr(&mut self, element: Expr, element_range: TextRange) -> ExprWithRange {
        let (generators, range) = self.parse_generators(element_range);

        (
            Expr::GeneratorExp(ast::ExprGeneratorExp {
                elt: Box::new(element),
                generators,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_list_comprehension_expr(
        &mut self,
        element: Expr,
        element_range: TextRange,
    ) -> ExprWithRange {
        let (generators, range) = self.parse_generators(element_range);

        (
            Expr::ListComp(ast::ExprListComp {
                elt: Box::new(element),
                generators,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_dict_comprehension_expr(
        &mut self,
        key: Expr,
        value: Expr,
        range: TextRange,
    ) -> ExprWithRange {
        let (generators, range) = self.parse_generators(range);

        (
            Expr::DictComp(ast::ExprDictComp {
                key: Box::new(key),
                value: Box::new(value),
                generators,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_set_comprehension_expr(
        &mut self,
        element: Expr,
        element_range: TextRange,
    ) -> ExprWithRange {
        let (generators, range) = self.parse_generators(element_range);

        (
            Expr::SetComp(ast::ExprSetComp {
                elt: Box::new(element),
                generators,
                range,
            })
            .into(),
            range,
        )
    }

    fn parse_starred_expr(&mut self, (_, range): Spanned) -> ExprWithRange {
        let (parsed_expr, expr_range) = self.parse_expr();
        let star_range = range.cover(expr_range);

        (
            Expr::Starred(ast::ExprStarred {
                value: Box::new(parsed_expr.expr),
                ctx: ExprContext::Load,
                range: star_range,
            })
            .into(),
            star_range,
        )
    }

    fn parse_await_expr(&mut self, start_range: TextRange) -> ExprWithRange {
        let mut await_range = start_range;

        let (parsed_expr, expr_range) = self.expr_bp(19);
        await_range = await_range.cover(expr_range);

        if matches!(parsed_expr.expr, Expr::Starred(_)) {
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "starred expression `{}` is not allowed in an `await` statement",
                    self.src_text(expr_range)
                )),
                expr_range,
            );
        }

        (
            Expr::Await(ast::ExprAwait {
                value: Box::new(parsed_expr.expr),
                range: await_range,
            })
            .into(),
            await_range,
        )
    }

    fn parse_yield_expr(&mut self, mut yield_range: TextRange) -> ExprWithRange {
        if self.eat(TokenKind::From) {
            return self.parse_yield_from_expr(yield_range);
        }

        let value = if self.at_expr() {
            let (parsed_expr, expr_range) = self.parse_exprs();
            yield_range = yield_range.cover(expr_range);

            Some(Box::new(parsed_expr.expr))
        } else {
            None
        };

        (
            Expr::Yield(ast::ExprYield {
                value,
                range: yield_range,
            })
            .into(),
            yield_range,
        )
    }

    fn parse_yield_from_expr(&mut self, mut yield_range: TextRange) -> ExprWithRange {
        let (parsed_expr, expr_range) = self.parse_exprs();
        yield_range = yield_range.cover(expr_range);

        match parsed_expr.expr {
            Expr::Starred(_) => {
                // Should we make `expr` an `Expr::Invalid` here?
                self.add_error(
                    ParseErrorType::OtherError(format!(
                        "starred expression `{}` is not allowed in a `yield from` statement",
                        self.src_text(expr_range)
                    )),
                    expr_range,
                );
            }
            Expr::Tuple(_) if !parsed_expr.is_parenthesized => {
                // Should we make `expr` an `Expr::Invalid` here?
                self.add_error(
                    ParseErrorType::OtherError(format!(
                        "unparenthesized tuple `{}` is not allowed in a `yield from` statement",
                        self.src_text(expr_range)
                    )),
                    expr_range,
                );
            }
            _ => {}
        }

        (
            Expr::YieldFrom(ast::ExprYieldFrom {
                value: Box::new(parsed_expr.expr),
                range: yield_range,
            })
            .into(),
            yield_range,
        )
    }

    fn parse_if_expr(&mut self, body: Expr, body_range: TextRange) -> ExprWithRange {
        self.bump(TokenKind::If);

        let (test, _) = self.parse_expr_simple();

        self.expect_and_recover(TokenKind::Else, TokenSet::EMPTY);

        let (orelse, orelse_range) = self.parse_expr_with_recovery(
            Parser::parse_expr,
            TokenSet::EMPTY,
            "expecting expression after `else` keyword",
        );
        let if_range = body_range.cover(orelse_range);

        (
            Expr::IfExp(ast::ExprIfExp {
                body: Box::new(body),
                test: Box::new(test.expr),
                orelse: Box::new(orelse.expr),
                range: if_range,
            })
            .into(),
            if_range,
        )
    }

    fn parse_lambda_expr(&mut self, start_range: TextRange) -> ExprWithRange {
        let mut lambda_range = start_range;

        let parameters: Option<Box<ast::Parameters>> = if self.at(TokenKind::Colon) {
            None
        } else {
            Some(Box::new(self.parse_parameters(FunctionKind::Lambda)))
        };

        self.expect_and_recover(TokenKind::Colon, TokenSet::EMPTY);

        // Check for forbidden tokens in the `lambda`'s body
        let (kind, range) = self.current_token();
        match kind {
            TokenKind::Yield => self.add_error(
                ParseErrorType::OtherError(
                    "`yield` not allowed in a `lambda` expression".to_string(),
                ),
                range,
            ),
            TokenKind::Star => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "starred expression not allowed in a `lambda` expression".to_string(),
                    ),
                    range,
                );
            }
            TokenKind::DoubleStar => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "double starred expression not allowed in a `lambda` expression"
                            .to_string(),
                    ),
                    range,
                );
            }
            _ => {}
        }

        let (body, body_range) = self.parse_expr();
        lambda_range = lambda_range.cover(body_range);

        (
            Expr::Lambda(ast::ExprLambda {
                body: Box::new(body.expr),
                parameters,
                range: lambda_range,
            })
            .into(),
            lambda_range,
        )
    }

    fn parse_parameter(&mut self, function_kind: FunctionKind) -> ast::Parameter {
        let start = self.node_start();
        let name = self.parse_identifier();
        // If we are at a colon and we're currently parsing a `lambda` expression,
        // this is the `lambda`'s body, don't try to parse as an annotation.
        let annotation = if function_kind != FunctionKind::Lambda && self.eat(TokenKind::Colon) {
            let (ann, _) = self.parse_expr();
            Some(Box::new(ann.expr))
        } else {
            None
        };

        ast::Parameter {
            range: self.node_range(start),
            name,
            annotation,
        }
    }

    fn parse_parameter_with_default(
        &mut self,
        function_kind: FunctionKind,
    ) -> ast::ParameterWithDefault {
        let start = self.node_start();
        let parameter = self.parse_parameter(function_kind);

        let default = if self.eat(TokenKind::Equal) {
            let (parsed_expr, _) = self.parse_expr();
            Some(Box::new(parsed_expr.expr))
        } else {
            None
        };

        ast::ParameterWithDefault {
            range: self.node_range(start),
            parameter,
            default,
        }
    }

    fn parse_parameters(&mut self, function_kind: FunctionKind) -> ast::Parameters {
        let mut args = vec![];
        let mut posonlyargs = vec![];
        let mut kwonlyargs = vec![];
        let mut kwarg = None;
        let mut vararg = None;

        let mut has_seen_asterisk = false;
        let mut has_seen_vararg = false;
        let mut has_seen_default_param = false;

        let ending = match function_kind {
            FunctionKind::Lambda => TokenKind::Colon,
            FunctionKind::FunctionDef => TokenKind::Rpar,
        };

        let ending_set = TokenSet::new(&[TokenKind::Rarrow, ending]).union(COMPOUND_STMT_SET);
        let start = self.node_start();

        self.parse_separated(true, TokenKind::Comma, ending_set, |parser| {
            // Don't allow any parameter after we have seen a vararg `**kwargs`
            if has_seen_vararg {
                parser.add_error(
                    ParseErrorType::ParamFollowsVarKeywordParam,
                    parser.current_range(),
                );
            }

            if parser.eat(TokenKind::Star) {
                has_seen_asterisk = true;
                if parser.at(TokenKind::Comma) {
                    has_seen_default_param = false;
                } else if parser.at_expr() {
                    let param = parser.parse_parameter(function_kind);
                    vararg = Some(Box::new(param));
                }
            } else if parser.eat(TokenKind::DoubleStar) {
                has_seen_vararg = true;
                let param = parser.parse_parameter(function_kind);
                kwarg = Some(Box::new(param));
            } else if parser.eat(TokenKind::Slash) {
                // Don't allow `/` after a `*`
                if has_seen_asterisk {
                    parser.add_error(
                        ParseErrorType::OtherError("`/` must be ahead of `*`".to_string()),
                        parser.current_range(),
                    );
                }
                std::mem::swap(&mut args, &mut posonlyargs);
            } else if parser.at(TokenKind::Name) {
                let param = parser.parse_parameter_with_default(function_kind);
                // Don't allow non-default parameters after default parameters e.g. `a=1, b`,
                // can't place `b` after `a=1`. Non-default parameters are only allowed after
                // default parameters if we have a `*` before them, e.g. `a=1, *, b`.
                if param.default.is_none() && has_seen_default_param && !has_seen_asterisk {
                    parser.add_error(ParseErrorType::DefaultArgumentError, parser.current_range());
                }
                has_seen_default_param = param.default.is_some();

                if has_seen_asterisk {
                    kwonlyargs.push(param);
                } else {
                    args.push(param);
                }
            } else {
                if parser.at_ts(SIMPLE_STMT_SET) {
                    return TextRange::default(); // We can return any range here
                }

                let range = parser.current_range();
                parser.skip_until(
                    ending_set.union([TokenKind::Comma, TokenKind::Colon].as_slice().into()),
                );
                parser.add_error(
                    ParseErrorType::OtherError("expected parameter".to_string()),
                    range.cover(parser.current_range()), // TODO(micha): This goes one token too far?
                );
            }

            // TODO(micha): Remove
            TextRange::default()
        });

        let parameters = ast::Parameters {
            range: self.node_range(start),
            posonlyargs,
            args,
            vararg,
            kwonlyargs,
            kwarg,
        };

        if let Err(error) = helpers::validate_parameters(&parameters) {
            self.errors.push(error);
        }

        parameters
    }

    fn parse_named_expr(&mut self, mut target: Expr, target_range: TextRange) -> ExprWithRange {
        assert!(self.eat(TokenKind::ColonEqual));

        if !helpers::is_valid_assignment_target(&target) {
            self.add_error(ParseErrorType::NamedAssignmentError, target_range);
        }
        helpers::set_expr_ctx(&mut target, ExprContext::Store);

        let (value, value_range) = self.parse_expr();
        let range = target_range.cover(value_range);

        (
            Expr::NamedExpr(ast::ExprNamedExpr {
                target: Box::new(target),
                value: Box::new(value.expr),
                range,
            })
            .into(),
            range,
        )
    }
}
