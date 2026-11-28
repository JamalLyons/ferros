//! Parse and evaluate `print <path>`: a variable followed by field access, indexing and
//! dereference.
//!
//! The grammar, loosest first:
//!
//! ```text
//! path    := '*' path | postfix
//! postfix := primary ( '.' name | '.' integer | '[' index ']' )*
//! primary := name | '(' path ')'
//! index   := an integer literal: decimal, 0x hex, 0o octal or 0b binary, with `_` separators
//! ```
//!
//! As in Rust, `*a.b` dereferences `a.b`; write `(*a).b` to dereference `a` first. `.0`
//! selects a tuple or tuple-struct field. On a Rust enum, `.field` and `.0` select a field
//! of the active variant, read from the discriminant (an explicit tag or a niche) as the
//! enum printer does; a field only another variant has is an error naming both. `[n]`
//! indexes an array, a slice (`&[T]`, `Box<[T]>`, `*p` for a raw `*const [T]`), a `Vec`,
//! a `VecDeque` in logical order, or the bytes of a `String` or `str`.
//!
//! `*` follows a reference, a raw pointer, a `Box`, an `Rc` or `Arc` (to its payload), a
//! live `Weak` (a dangling one is an error), a `Pin` (through the pointer it pins), a
//! `Cell` or `RefCell` (to the value inside) and a `Cow` (to the value it borrows or
//! owns). Dereferencing a slice, `Vec` or string gives its unsized contents, which can be
//! indexed but not printed alone. Field access and indexing look through all of these
//! except `Weak` and raw pointers, as Rust's auto-dereference does; a `Cow`'s `.0` still
//! selects its variant's field. Raw pointers need an explicit `*`. There are no function
//! calls, operators or literals other than an index.
//!
//! Evaluation reads only what the path needs: the root variable's bytes, the pointers and
//! headers it follows, and the bytes of the final value, which the caller formats.
//!
//! # Map of this file
//!
//! - `Path`, `Expr` and their `Display`: the parsed path.
//! - `PathError`: parse and evaluation errors, naming the failing segment.
//! - `Token`, `tokenize`, `Parser`, `parse_index`, `parse_field_index`: the parser.
//! - `Storage`, `Place`, `evaluate`: the evaluated value the caller formats.
//! - `Evaluator`, `Value`, `Slice`, `Shape`: one step per path segment (`field`,
//!   `index`, `deref`, `auto_deref`) and the wrapper layouts they follow.
//! - `member_named`, `raw_pointer_hint`, `check_bounds`, `child`: helpers.

use std::fmt;

use gimli::RunTimeEndian;

use super::format::{ValueView, unsigned};
use super::location::value_size;
use super::pretty::{
    SharedPointee, SharedTarget, fat_pointer, follow_shared, ring_layout, variant_for_tag, variant_payload, vec_layout,
};
use super::types::{Member, PointerKind, RustEnum, TypeId, TypeKind, TypeTable, Variant};
use crate::error::Result as DebuggerResult;
use crate::types::Address;

const MAX_TOKENS: usize = 256;

/// A parsed `print` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path
{
    expr: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expr
{
    Variable(String),
    Field(Box<Expr>, String),
    /// The index and its literal as written, so errors echo `v[0x10]`.
    Index(Box<Expr>, u64, String),
    Deref(Box<Expr>),
}

impl Path
{
    /// Parse `text`.
    ///
    /// # Errors
    ///
    /// A [`PathError`] naming the token that is not part of the grammar, including
    /// function calls and operators.
    pub fn parse(text: &str) -> Result<Self, PathError>
    {
        let tokens = tokenize(text)?;
        // The parser and evaluator recurse once per token at most.
        if tokens.len() > MAX_TOKENS {
            return Err(PathError::syntax(format!("path is longer than {MAX_TOKENS} tokens")));
        }
        let mut parser = Parser { tokens, next: 0 };
        let expr = parser.path()?;
        if let Some(&(token, column)) = parser.tokens.get(parser.next) {
            return Err(unexpected(token, column));
        }
        Ok(Self { expr })
    }

    /// The variable the path starts from.
    #[must_use]
    pub fn root(&self) -> &str
    {
        let mut expr = &self.expr;
        loop {
            match expr {
                Expr::Variable(name) => return name,
                Expr::Field(base, _) | Expr::Index(base, ..) | Expr::Deref(base) => expr = base,
            }
        }
    }

    /// Whether the path is only a variable name.
    #[must_use]
    pub const fn is_variable(&self) -> bool
    {
        matches!(self.expr, Expr::Variable(_))
    }
}

impl fmt::Display for Path
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        self.expr.fmt(f)
    }
}

impl fmt::Display for Expr
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match self {
            Self::Variable(name) => f.write_str(name),
            Self::Field(base, name) => {
                write_base(f, base)?;
                write!(f, ".{name}")
            }
            Self::Index(base, _, literal) => {
                write_base(f, base)?;
                write!(f, "[{literal}]")
            }
            Self::Deref(inner) => write!(f, "*{inner}"),
        }
    }
}

fn write_base(f: &mut fmt::Formatter<'_>, base: &Expr) -> fmt::Result
{
    match base {
        Expr::Deref(_) => write!(f, "({base})"),
        _ => fmt::Display::fmt(base, f),
    }
}

/// Why a path could not be parsed or evaluated. Evaluation errors name the segment that
/// failed, written as the path up to and including it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError
{
    /// The failing prefix of the path, when evaluation failed.
    pub segment: Option<String>,
    /// What went wrong.
    pub reason: String,
}

impl PathError
{
    fn syntax(reason: impl Into<String>) -> Self
    {
        Self {
            segment: None,
            reason: reason.into(),
        }
    }

    pub(crate) fn root(path: &Path, reason: impl Into<String>) -> Self
    {
        Self {
            segment: Some(path.root().to_owned()),
            reason: reason.into(),
        }
    }

    fn at(expr: &Expr, reason: impl Into<String>) -> Self
    {
        Self {
            segment: Some(expr.to_string()),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for PathError
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match &self.segment {
            Some(segment) => write!(f, "`{segment}`: {}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

impl std::error::Error for PathError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a>
{
    Name(&'a str),
    Integer(&'a str),
    Dot,
    Star,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,
    Other(char),
}

impl fmt::Display for Token<'_>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match self {
            Self::Name(text) | Self::Integer(text) => f.write_str(text),
            Self::Dot => f.write_str("."),
            Self::Star => f.write_str("*"),
            Self::OpenParen => f.write_str("("),
            Self::CloseParen => f.write_str(")"),
            Self::OpenBracket => f.write_str("["),
            Self::CloseBracket => f.write_str("]"),
            Self::Other(ch) => write!(f, "{ch}"),
        }
    }
}

/// Tokens with their 1-based column.
fn tokenize(text: &str) -> Result<Vec<(Token<'_>, usize)>, PathError>
{
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let column = text[..start].chars().count() + 1;
        let token = match ch {
            ch if ch.is_whitespace() => continue,
            '.' => Token::Dot,
            '*' => Token::Star,
            '(' => Token::OpenParen,
            ')' => Token::CloseParen,
            '[' => Token::OpenBracket,
            ']' => Token::CloseBracket,
            ch if ch.is_ascii_digit() || ch == '_' || ch.is_alphabetic() => {
                let mut end = start + ch.len_utf8();
                while let Some(&(next, ch)) = chars.peek() {
                    if !(ch.is_alphanumeric() || ch == '_') {
                        break;
                    }
                    end = next + ch.len_utf8();
                    chars.next();
                }
                let word = &text[start..end];
                if ch.is_ascii_digit() {
                    Token::Integer(word)
                } else {
                    Token::Name(word)
                }
            }
            other => Token::Other(other),
        };
        tokens.push((token, column));
    }
    if tokens.is_empty() {
        return Err(PathError::syntax("expected a variable name"));
    }
    Ok(tokens)
}

fn unexpected(token: Token<'_>, column: usize) -> PathError
{
    let detail = match token {
        Token::Other('+' | '-' | '/' | '%' | '&' | '|' | '^' | '!' | '<' | '>' | '=' | '?') => {
            ": operators are not supported"
        }
        Token::OpenParen => ": function calls are not supported",
        _ => "",
    };
    PathError::syntax(format!("unexpected `{token}` at column {column}{detail}"))
}

struct Parser<'a>
{
    tokens: Vec<(Token<'a>, usize)>,
    next: usize,
}

impl<'a> Parser<'a>
{
    fn peek(&self) -> Option<Token<'a>>
    {
        self.tokens.get(self.next).map(|&(token, _)| token)
    }

    fn take(&mut self) -> Result<(Token<'a>, usize), PathError>
    {
        let token = self
            .tokens
            .get(self.next)
            .copied()
            .ok_or_else(|| PathError::syntax("unexpected end of path"))?;
        self.next += 1;
        Ok(token)
    }

    fn path(&mut self) -> Result<Expr, PathError>
    {
        if self.peek() == Some(Token::Star) {
            self.next += 1;
            return Ok(Expr::Deref(Box::new(self.path()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, PathError>
    {
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.next += 1;
                    let name = match self.take()? {
                        (Token::Name(name), _) => name.to_owned(),
                        (Token::Integer(index), column) => {
                            parse_field_index(index, column)?;
                            index.to_owned()
                        }
                        (token, column) => return Err(unexpected(token, column)),
                    };
                    expr = Expr::Field(Box::new(expr), name);
                }
                Some(Token::OpenBracket) => {
                    self.next += 1;
                    let (index, literal) = match self.take()? {
                        (Token::Integer(literal), column) => (parse_index(literal, column)?, literal.to_owned()),
                        (token, column) => {
                            return Err(PathError::syntax(format!(
                                "unexpected `{token}` at column {column}: an index is a non-negative integer"
                            )));
                        }
                    };
                    match self.take()? {
                        (Token::CloseBracket, _) => {}
                        (token, column) => return Err(unexpected(token, column)),
                    }
                    expr = Expr::Index(Box::new(expr), index, literal);
                }
                Some(Token::OpenParen) => {
                    let (token, column) = self.take()?;
                    return Err(unexpected(token, column));
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, PathError>
    {
        match self.take()? {
            (Token::Name(name), _) => Ok(Expr::Variable(name.to_owned())),
            (Token::OpenParen, _) => {
                let expr = self.path()?;
                match self.take()? {
                    (Token::CloseParen, _) => Ok(expr),
                    (token, column) => Err(unexpected(token, column)),
                }
            }
            (token @ Token::Integer(_), column) => Err(PathError::syntax(format!(
                "unexpected `{token}` at column {column}: a path starts with a variable name"
            ))),
            (token, column) => Err(unexpected(token, column)),
        }
    }
}

/// An index literal as Rust spells integers: decimal, `0x`, `0o` or `0b`, with `_`
/// separators after the first digit.
fn parse_index(text: &str, column: usize) -> Result<u64, PathError>
{
    let invalid = || PathError::syntax(format!("invalid index `{text}` at column {column}"));
    let (radix, digits) = match text.get(..2) {
        Some("0x") => (16, &text[2..]),
        Some("0o") => (8, &text[2..]),
        Some("0b") => (2, &text[2..]),
        _ => (10, text),
    };
    let digits: String = digits.chars().filter(|&ch| ch != '_').collect();
    // from_str_radix accepts a leading sign, which no index token holds but `0x+1` might.
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_digit(radix)) {
        return Err(invalid());
    }
    u64::from_str_radix(&digits, radix).map_err(|_| invalid())
}

/// A tuple field index: plain decimal digits, as Rust requires after `.`.
fn parse_field_index(text: &str, column: usize) -> Result<u64, PathError>
{
    text.parse()
        .map_err(|_| PathError::syntax(format!("invalid field index `{text}` at column {column}")))
}

/// Where an evaluated value's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Storage
{
    /// Bytes already read, with the address of the first when memory-backed.
    Bytes(Vec<u8>, Option<Address>),
    /// Unread target memory at this runtime address.
    Memory(Address),
}

/// A typed value a path selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place
{
    pub(crate) ty: TypeId,
    pub(crate) storage: Storage,
}

/// Evaluate `path` from `root`, the value of [`Path::root`], in one image's types.
pub(crate) fn evaluate(
    path: &Path,
    root: Place,
    types: &TypeTable,
    endian: RunTimeEndian,
    read: &mut dyn FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
) -> Result<Place, PathError>
{
    let mut evaluator = Evaluator { types, endian, read };
    match evaluator.eval(&path.expr, Value::Place(root))? {
        Value::Place(place) => Ok(place),
        Value::Slice(slice) => {
            // Only a dereference produces unsized contents.
            let whole = match &path.expr {
                Expr::Deref(inner) => format!("print `{inner}` whole or "),
                _ => String::new(),
            };
            Err(PathError::at(
                &path.expr,
                format!("{} is unsized; {whole}index it", evaluator.slice_name(&slice)),
            ))
        }
    }
}

struct Evaluator<'a>
{
    types: &'a TypeTable,
    endian: RunTimeEndian,
    read: &'a mut dyn FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
}

/// A value part way through a path: a typed place, or the unsized contents of a slice or
/// string, which DWARF has no type for.
#[derive(Clone)]
enum Value
{
    Place(Place),
    Slice(Slice),
}

/// `len` elements of `element` at `data`. `text` marks the bytes of a `str`.
#[derive(Clone, Copy)]
struct Slice
{
    data: Address,
    len: u64,
    element: TypeId,
    text: bool,
}

/// What a value is to the path operations, after typedefs and qualifiers.
enum Shape
{
    Members,
    Array(TypeId, u64),
    /// A slice reference, `Box<[T]>`, `Vec`, `String` or string reference.
    Elements,
    VecDeque,
    Pointer(PointerKind, Option<TypeId>),
    /// `*const [T]`, `*mut [T]`, `*const str` or `*mut str`.
    RawSlice,
    /// `Rc` or `Arc`, or with `weak`, their `Weak`.
    Shared
    {
        weak: bool,
    },
    Pin,
    Cell,
    Cow,
    Other,
}

const STRING_NAME: &str = "alloc::string::String";
const SHARED_NAMES: &[&str] = &["alloc::rc::Rc<", "alloc::sync::Arc<"];
const WEAK_NAMES: &[&str] = &["alloc::rc::Weak<", "alloc::sync::Weak<"];
const CELL_NAMES: &[&str] = &["core::cell::Cell<", "core::cell::RefCell<"];
const RAW_SLICE_NAMES: &[&str] = &["*const [", "*mut [", "*const str", "*mut str"];
const PIN_NAME: &str = "core::pin::Pin<";
const COW_NAME: &str = "alloc::borrow::Cow<";
const VEC_DEQUE_NAME: &str = "alloc::collections::vec_deque::VecDeque<";

fn starts_with_any(name: &str, prefixes: &[&str]) -> bool
{
    prefixes.iter().any(|prefix| name.starts_with(prefix))
}

impl Evaluator<'_>
{
    fn eval(&mut self, expr: &Expr, root: Value) -> Result<Value, PathError>
    {
        match expr {
            Expr::Variable(_) => Ok(root),
            Expr::Field(base, name) => {
                let value = self.eval(base, root)?;
                self.field_through(expr, base, value, name, 16).map(Value::Place)
            }
            Expr::Index(base, index, _) => {
                let value = self.eval(base, root)?;
                let value = self.auto_deref(base, value)?;
                self.index(expr, value, *index).map(Value::Place)
            }
            Expr::Deref(inner) => {
                let value = self.eval(inner, root)?;
                self.deref(expr, value)
            }
        }
    }

    fn type_name(&self, ty: TypeId) -> String
    {
        self.types
            .get(ty)
            .and_then(|ty| ty.name.clone())
            .unwrap_or_else(|| "a value of unknown type".to_owned())
    }

    fn slice_name(&self, slice: &Slice) -> String
    {
        if slice.text {
            "str".to_owned()
        } else {
            format!("[{}]", self.type_name(slice.element))
        }
    }

    fn shape(&self, ty: TypeId) -> Shape
    {
        let Some(ty) = self.types.get(self.types.strip_aliases(ty)) else {
            return Shape::Other;
        };
        let name = ty.name.as_deref().unwrap_or_default();
        match &ty.kind {
            TypeKind::Pointer(pointer) => Shape::Pointer(pointer.kind, pointer.pointee),
            TypeKind::Array(array) => match (array.element, array.lengths.as_slice()) {
                (Some(element), [Some(len)]) => Shape::Array(element, *len),
                _ => Shape::Other,
            },
            TypeKind::Enum(_) if name.starts_with(COW_NAME) => Shape::Cow,
            TypeKind::Struct(_)
                if name.starts_with("alloc::vec::Vec<")
                    || name == STRING_NAME
                    || name.starts_with("&[")
                    || name.starts_with("&mut [")
                    || matches!(name, "&str" | "&mut str")
                    || name
                        .strip_prefix("alloc::boxed::Box<")
                        .is_some_and(|boxed| boxed.starts_with('[') || boxed == "str>" || boxed.starts_with("str,")) =>
            {
                Shape::Elements
            }
            TypeKind::Struct(_) if name.starts_with(VEC_DEQUE_NAME) => Shape::VecDeque,
            TypeKind::Struct(_) if starts_with_any(name, RAW_SLICE_NAMES) => Shape::RawSlice,
            TypeKind::Struct(_) if starts_with_any(name, SHARED_NAMES) => Shape::Shared { weak: false },
            TypeKind::Struct(_) if starts_with_any(name, WEAK_NAMES) => Shape::Shared { weak: true },
            TypeKind::Struct(_) if name.starts_with(PIN_NAME) => Shape::Pin,
            TypeKind::Struct(_) if starts_with_any(name, CELL_NAMES) => Shape::Cell,
            TypeKind::Struct(_) | TypeKind::Tuple(_) | TypeKind::Union(_) if !ty.declaration => Shape::Members,
            _ => Shape::Other,
        }
    }

    /// One automatic dereference: through a reference, a `Box`, an `Rc` or `Arc`, a `Pin`,
    /// a cell or a `Cow`. `None` when `value` is none of those.
    fn auto_deref_step(&mut self, expr: &Expr, value: &Value) -> Option<Result<Value, PathError>>
    {
        let Value::Place(place) = value else {
            return None;
        };
        Some(match self.shape(place.ty) {
            Shape::Pointer(PointerKind::Reference { .. } | PointerKind::Box, Some(_)) | Shape::Shared { weak: false } => {
                self.deref(expr, value.clone())
            }
            Shape::Pin => self.pinned_pointer(expr, place).map(Value::Place),
            Shape::Cell => self.cell_value(expr, place).map(Value::Place),
            Shape::Cow => self.cow_payload(expr, place).map(|(payload, _)| Value::Place(payload)),
            _ => return None,
        })
    }

    /// Look through every automatic dereference to the value an index applies to.
    fn auto_deref(&mut self, expr: &Expr, mut value: Value) -> Result<Value, PathError>
    {
        // Bound the walk so a malformed self-referential pointer type cannot loop.
        for _ in 0..16 {
            match self.auto_deref_step(expr, &value) {
                Some(next) => value = next?,
                None => break,
            }
        }
        Ok(value)
    }

    /// Field `name` of `value` or of what it automatically dereferences to. As in Rust, the
    /// innermost value with the field wins, so `rc.name` is the payload's `name`; a field
    /// only the wrapper has, such as a `Cell`'s `value`, a `Pin`'s `pointer` or a `Cow`'s
    /// `.0`, is the wrapper's. An error names the innermost value.
    fn field_through(&mut self, expr: &Expr, base: &Expr, value: Value, name: &str, depth: usize)
    -> Result<Place, PathError>
    {
        let inner = match self.auto_deref_step(base, &value) {
            Some(inner) if depth > 0 => inner,
            _ => return self.field(expr, value, name),
        };
        let error = match inner.and_then(|inner| self.field_through(expr, base, inner, name, depth - 1)) {
            Ok(place) => return Ok(place),
            Err(error) => error,
        };
        self.field(expr, value, name).map_err(|_| error)
    }

    fn field(&mut self, expr: &Expr, value: Value, name: &str) -> Result<Place, PathError>
    {
        let place = match value {
            Value::Place(place) => place,
            Value::Slice(slice) => {
                return Err(PathError::at(expr, format!("{} has no fields", self.slice_name(&slice))));
            }
        };
        let ty = self.types.strip_aliases(place.ty);
        let fields = match self.types.get(ty).map(|ty| (ty.kind.clone(), ty.declaration)) {
            Some((TypeKind::Struct(fields) | TypeKind::Tuple(fields) | TypeKind::Union(fields), false)) => fields,
            Some((TypeKind::Enum(layout), _)) => return self.variant_field(expr, place, ty, &layout, name),
            _ => {
                let hint = raw_pointer_hint(expr, &self.shape(ty));
                return Err(PathError::at(expr, format!("{} has no fields{hint}", self.type_name(ty))));
            }
        };
        let member = member_named(&fields.members, name).ok_or_else(|| {
            let hint = raw_pointer_hint(expr, &self.shape(ty));
            PathError::at(expr, format!("no field `{name}` in {}{hint}", self.type_name(ty)))
        })?;
        self.member_place(expr, &place, member, 0)
    }

    /// Field `name` of the variant a Rust enum holds. A field only another variant has
    /// is an error naming both variants.
    fn variant_field(
        &mut self,
        expr: &Expr,
        place: Place,
        ty: TypeId,
        layout: &RustEnum,
        name: &str,
    ) -> Result<Place, PathError>
    {
        // Read only the discriminant, so a large enum whose active variant is small stays
        // inside the value cap.
        // A discriminant without a readable layout falls through to the shared error.
        let tag = match layout
            .discriminant
            .as_ref()
            .and_then(|discriminant| self.member_place(expr, &place, discriminant, 0).ok())
        {
            Some(tag) => Some(self.load(expr, &tag)?),
            None => None,
        };
        let variant = variant_for_tag(layout, tag.as_deref(), self.endian)
            .map_err(|error| PathError::at(expr, format!("cannot read the variant of {}: {error}", self.type_name(ty))))?;
        let enum_name = self.types.get(ty).and_then(|ty| ty.name.clone());
        let enum_name = enum_name.as_deref();
        // A variant's field and the offset its member offset is from: the payload struct's
        // own member, when rustc wraps the fields in one.
        let find = |variant: &Variant| -> Option<(Member, Option<u64>)> {
            match variant_payload(variant, enum_name, self.types) {
                Some(payload) => Some((member_named(payload.fields(), name)?.clone(), payload.member.offset)),
                None => Some((member_named(&variant.members, name)?.clone(), Some(0))),
            }
        };
        if let Some((member, base)) = find(variant) {
            let base = base.ok_or_else(|| PathError::at(expr, "field layout unknown"))?;
            return self.member_place(expr, &place, &member, base);
        }
        let variant_name = |variant: &Variant| variant.name.clone().unwrap_or_else(|| "?".to_owned());
        let others: Vec<_> = layout
            .variants
            .iter()
            .filter(|other| !std::ptr::eq(*other, variant) && find(other).is_some())
            .map(variant_name)
            .collect();
        let active = variant_name(variant);
        let reason = if others.is_empty() {
            format!("no field `{name}` in {}::{active}", self.type_name(ty))
        } else {
            format!("{} is {active}, not {}", self.type_name(ty), others.join(" or "))
        };
        Err(PathError::at(expr, reason))
    }

    /// The value of `member`, whose offset is from `base` bytes into `place`.
    fn member_place(&self, expr: &Expr, place: &Place, member: &Member, base: u64) -> Result<Place, PathError>
    {
        let missing = || PathError::at(expr, "field layout unknown");
        if member.bit_size.is_some() {
            return Err(PathError::at(expr, "bit fields are not supported"));
        }
        let member_ty = member.ty.ok_or_else(missing)?;
        let offset = member
            .offset
            .and_then(|offset| offset.checked_add(base))
            .ok_or_else(missing)?;
        let size = self.types.get(member_ty).and_then(|ty| ty.size).ok_or_else(missing)?;
        child(expr, place, member_ty, offset, size)
    }

    /// The struct member `name` of `place`, whose layout the caller expects.
    fn named_member(&self, expr: &Expr, place: &Place, names: &[&str]) -> Result<Place, PathError>
    {
        let ty = self.types.strip_aliases(place.ty);
        let member = match self.types.get(ty).map(|ty| ty.kind.clone()) {
            Some(TypeKind::Struct(fields)) => names.iter().find_map(|name| fields.member(name).cloned()),
            _ => None,
        };
        let member =
            member.ok_or_else(|| PathError::at(expr, format!("unrecognized layout of {}", self.type_name(place.ty))))?;
        self.member_place(expr, place, &member, 0)
    }

    /// The pointer a `Pin` holds.
    fn pinned_pointer(&self, expr: &Expr, place: &Place) -> Result<Place, PathError>
    {
        self.named_member(expr, place, &["pointer", "__pointer"])
    }

    /// The value inside a `Cell` or `RefCell`: its `UnsafeCell`'s value.
    fn cell_value(&self, expr: &Expr, place: &Place) -> Result<Place, PathError>
    {
        let unsafe_cell = self.named_member(expr, place, &["value"])?;
        self.named_member(expr, &unsafe_cell, &["value"])
    }

    /// The field of a `Cow`'s active variant, and whether that variant is `Borrowed`.
    fn cow_payload(&mut self, expr: &Expr, place: &Place) -> Result<(Place, bool), PathError>
    {
        let ty = self.types.strip_aliases(place.ty);
        let Some(TypeKind::Enum(layout)) = self.types.get(ty).map(|ty| ty.kind.clone()) else {
            return Err(PathError::at(expr, format!("unrecognized layout of {}", self.type_name(ty))));
        };
        let tag = match layout
            .discriminant
            .as_ref()
            .and_then(|discriminant| self.member_place(expr, place, discriminant, 0).ok())
        {
            Some(tag) => Some(self.load(expr, &tag)?),
            None => None,
        };
        let variant = variant_for_tag(&layout, tag.as_deref(), self.endian)
            .map_err(|error| PathError::at(expr, format!("cannot read the variant of {}: {error}", self.type_name(ty))))?;
        let borrowed = variant.name.as_deref() == Some("Borrowed");
        let enum_name = self.types.get(ty).and_then(|ty| ty.name.clone());
        let (member, base) = match variant_payload(variant, enum_name.as_deref(), self.types) {
            Some(payload) => (member_named(payload.fields(), "0").cloned(), payload.member.offset),
            None => (member_named(&variant.members, "0").cloned(), Some(0)),
        };
        let (Some(member), Some(base)) = (member, base) else {
            return Err(PathError::at(expr, format!("unrecognized layout of {}", self.type_name(ty))));
        };
        Ok((self.member_place(expr, place, &member, base)?, borrowed))
    }

    fn index(&mut self, expr: &Expr, value: Value, index: u64) -> Result<Place, PathError>
    {
        let place = match value {
            Value::Place(place) => place,
            Value::Slice(slice) => return self.slice_element(expr, &slice, index),
        };
        match self.shape(place.ty) {
            Shape::Array(element, len) => {
                check_bounds(expr, index, len)?;
                let size = self.element_size(expr, element)?;
                let offset = index
                    .checked_mul(size)
                    .ok_or_else(|| PathError::at(expr, "element offset overflows"))?;
                child(expr, &place, element, offset, size)
            }
            Shape::Elements => {
                let slice = self.elements(expr, &place)?;
                self.slice_element(expr, &slice, index)
            }
            Shape::VecDeque => {
                let bytes = self.load(expr, &place)?;
                let view = ValueView {
                    ty: self.types.strip_aliases(place.ty),
                    bytes: &bytes,
                    address: None,
                };
                let ring = ring_layout(view, self.types, self.endian)
                    .ok_or_else(|| PathError::at(expr, format!("unrecognized layout of {}", self.type_name(place.ty))))?;
                ring.check().map_err(|reason| PathError::at(expr, reason))?;
                check_bounds(expr, index, ring.len)?;
                let address = ring.element_address(index).map_err(|reason| PathError::at(expr, reason))?;
                Ok(Place {
                    ty: ring.element,
                    storage: Storage::Memory(address),
                })
            }
            shape => {
                let hint = raw_pointer_hint(expr, &shape);
                Err(PathError::at(
                    expr,
                    format!("cannot index {}{hint}", self.type_name(place.ty)),
                ))
            }
        }
    }

    /// The contents of a slice reference, `Box<[T]>`, `Vec`, `String` or string reference.
    fn elements(&mut self, expr: &Expr, place: &Place) -> Result<Slice, PathError>
    {
        let bytes = self.load(expr, place)?;
        let ty = self.types.strip_aliases(place.ty);
        let view = ValueView {
            ty,
            bytes: &bytes,
            address: None,
        };
        let name = self.types.get(ty).and_then(|ty| ty.name.clone());
        let text = matches!(name.as_deref(), Some(STRING_NAME | "&str" | "&mut str"))
            || name
                .as_deref()
                .and_then(|name| name.strip_prefix("alloc::boxed::Box<"))
                .is_some_and(|boxed| boxed == "str>" || boxed.starts_with("str,"));
        // A String is a Vec<u8>.
        let vec = match self.types.get(ty).map(|ty| ty.kind.clone()) {
            Some(TypeKind::Struct(fields)) if name.as_deref() == Some(STRING_NAME) => fields
                .member("vec")
                .and_then(|member| view.member(member, self.types))
                .and_then(|vec| vec_layout(vec, self.types, self.endian)),
            _ => vec_layout(view, self.types, self.endian),
        };
        let layout = match vec {
            Some(vec) if vec.len > vec.capacity => {
                return Err(PathError::at(
                    expr,
                    format!("length {} exceeds capacity {}", vec.len, vec.capacity),
                ));
            }
            Some(vec) => Some((vec.address, vec.len, vec.element)),
            None => fat_pointer(view, self.types, self.endian),
        };
        let (data, len, element) =
            layout.ok_or_else(|| PathError::at(expr, format!("unrecognized layout of {}", self.type_name(place.ty))))?;
        Ok(Slice {
            data,
            len,
            element,
            text,
        })
    }

    fn slice_element(&self, expr: &Expr, slice: &Slice, index: u64) -> Result<Place, PathError>
    {
        check_bounds(expr, index, slice.len)?;
        if slice.data == Address::ZERO {
            return Err(PathError::at(expr, "null element data pointer"));
        }
        let size = self.element_size(expr, slice.element)?;
        let address = index
            .checked_mul(size)
            .and_then(|offset| slice.data.checked_add(offset))
            .ok_or_else(|| PathError::at(expr, "element address overflows"))?;
        Ok(Place {
            ty: slice.element,
            storage: Storage::Memory(address),
        })
    }

    fn deref(&mut self, expr: &Expr, value: Value) -> Result<Value, PathError>
    {
        let mut place = match value {
            Value::Place(place) => place,
            Value::Slice(slice) => {
                return Err(PathError::at(expr, format!("cannot dereference {}", self.slice_name(&slice))));
            }
        };
        // `*pin` dereferences the pointer the Pin holds. Bound nested pins.
        for _ in 0..16 {
            if !matches!(self.shape(place.ty), Shape::Pin) {
                break;
            }
            place = self.pinned_pointer(expr, &place)?;
        }
        let (kind, pointee) = match self.shape(place.ty) {
            Shape::Pointer(kind @ (PointerKind::Reference { .. } | PointerKind::Raw { .. } | PointerKind::Box), pointee) => {
                (kind, pointee)
            }
            Shape::Elements => return self.elements(expr, &place).map(Value::Slice),
            Shape::RawSlice => {
                // The pointer may dangle; only an index reads through it.
                let bytes = self.load(expr, &place)?;
                let view = ValueView {
                    ty: self.types.strip_aliases(place.ty),
                    bytes: &bytes,
                    address: None,
                };
                let (data, len, element) = fat_pointer(view, self.types, self.endian)
                    .ok_or_else(|| PathError::at(expr, format!("unrecognized layout of {}", self.type_name(place.ty))))?;
                if data == Address::ZERO {
                    return Err(PathError::at(expr, "null pointer"));
                }
                let text = self
                    .types
                    .get(self.types.strip_aliases(place.ty))
                    .and_then(|ty| ty.name.clone())
                    .is_some_and(|name| name.ends_with(" str"));
                return Ok(Value::Slice(Slice {
                    data,
                    len,
                    element,
                    text,
                }));
            }
            Shape::Shared { weak } => return self.follow_shared(expr, &place, weak),
            Shape::Cell => return self.cell_value(expr, &place).map(Value::Place),
            Shape::Cow => {
                // What Borrowed points to, or what Owned owns. A borrowed `str` or slice
                // stays its reference, which prints whole.
                let (payload, borrowed) = self.cow_payload(expr, &place)?;
                if borrowed && let Value::Place(target) = self.deref(expr, Value::Place(payload.clone()))? {
                    return Ok(Value::Place(target));
                }
                return Ok(Value::Place(payload));
            }
            _ => {
                return Err(PathError::at(
                    expr,
                    format!("cannot dereference {}", self.type_name(place.ty)),
                ));
            }
        };
        let pointee = pointee.ok_or_else(|| PathError::at(expr, "pointee type unknown"))?;
        let bytes = self.load(expr, &place)?;
        let address = (bytes.len() <= 8)
            .then(|| unsigned(&bytes, self.endian))
            .flatten()
            .and_then(|raw| u64::try_from(raw).ok())
            .ok_or_else(|| PathError::at(expr, "invalid pointer bytes"))?;
        if address == 0 {
            let what = match kind {
                PointerKind::Reference { .. } => "reference",
                PointerKind::Box => "Box pointer",
                _ => "pointer",
            };
            return Err(PathError::at(expr, format!("null {what}")));
        }
        Ok(Value::Place(Place {
            ty: pointee,
            storage: Storage::Memory(Address::new(address)),
        }))
    }

    /// The payload of an `Rc`, `Arc` or live `Weak`.
    fn follow_shared(&mut self, expr: &Expr, place: &Place, weak: bool) -> Result<Value, PathError>
    {
        let bytes = self.load(expr, place)?;
        let view = ValueView {
            ty: self.types.strip_aliases(place.ty),
            bytes: &bytes,
            address: None,
        };
        let types = self.types;
        let read = &mut *self.read;
        let mut read = |address, len| read(address, len).map_err(|error| error.to_string());
        let target = follow_shared(view, types, self.endian, &mut read)
            .ok_or_else(|| PathError::at(expr, format!("unrecognized layout of {}", self.type_name(place.ty))))?
            .map_err(|reason| PathError::at(expr, reason))?;
        let name = if weak { "Weak" } else { "shared" };
        let (address, pointee) = match target {
            SharedTarget::Null => return Err(PathError::at(expr, format!("null {name} pointer"))),
            SharedTarget::Dangling if weak => return Err(PathError::at(expr, "dangling Weak")),
            SharedTarget::Dangling => {
                return Err(PathError::at(expr, "shared allocation has no strong owners"));
            }
            SharedTarget::Live { address, pointee } => (address, pointee),
        };
        Ok(match pointee {
            SharedPointee::Sized(ty) => Value::Place(Place {
                ty,
                storage: Storage::Memory(address),
            }),
            SharedPointee::Slice(element, len) => Value::Slice(Slice {
                data: address,
                len,
                element,
                text: false,
            }),
            SharedPointee::Str(len) => Value::Slice(Slice {
                data: address,
                len,
                element: self
                    .types
                    .type_named("u8")
                    .ok_or_else(|| PathError::at(expr, "type u8 unknown"))?,
                text: true,
            }),
        })
    }

    fn element_size(&self, expr: &Expr, element: TypeId) -> Result<u64, PathError>
    {
        self.types
            .get(element)
            .and_then(|ty| ty.size)
            .ok_or_else(|| PathError::at(expr, "element size unknown"))
    }

    /// The whole value's bytes.
    fn load(&mut self, expr: &Expr, place: &Place) -> Result<Vec<u8>, PathError>
    {
        match &place.storage {
            Storage::Bytes(bytes, _) => Ok(bytes.clone()),
            Storage::Memory(address) => {
                let size = value_size(self.types.get(place.ty).and_then(|ty| ty.size))
                    .map_err(|reason| PathError::at(expr, reason.to_string()))?;
                let bytes = (self.read)(*address, size).map_err(|error| PathError::at(expr, error.to_string()))?;
                if bytes.len() != size {
                    return Err(PathError::at(
                        expr,
                        format!("expected {size} bytes at {address}, read {}", bytes.len()),
                    ));
                }
                Ok(bytes)
            }
        }
    }
}

/// The member named `name`. Rust names tuple and tuple-struct fields `__0`, `__1`, ...
fn member_named<'m>(members: &'m [Member], name: &str) -> Option<&'m Member>
{
    let named = |wanted: &str| members.iter().find(|member| member.name.as_deref() == Some(wanted));
    named(name).or_else(|| {
        name.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| named(&format!("__{name}")))
            .flatten()
    })
}

/// Raw pointers and `Weak` are not dereferenced implicitly. Suggest the explicit form.
fn raw_pointer_hint(expr: &Expr, shape: &Shape) -> String
{
    let what = match shape {
        Shape::Pointer(PointerKind::Raw { .. }, _) | Shape::RawSlice => "raw pointer",
        Shape::Shared { weak: true } => "Weak",
        _ => return String::new(),
    };
    let explicit = match expr {
        Expr::Field(base, name) => Expr::Field(Box::new(Expr::Deref(base.clone())), name.clone()),
        Expr::Index(base, index, literal) => Expr::Index(Box::new(Expr::Deref(base.clone())), *index, literal.clone()),
        _ => return String::new(),
    };
    format!("; dereference the {what} first, as in `{explicit}`")
}

fn check_bounds(expr: &Expr, index: u64, len: u64) -> Result<(), PathError>
{
    if index >= len {
        return Err(PathError::at(
            expr,
            format!("index {index} is out of bounds for length {len}"),
        ));
    }
    Ok(())
}

fn child(expr: &Expr, place: &Place, ty: TypeId, offset: u64, size: u64) -> Result<Place, PathError>
{
    let storage = match &place.storage {
        Storage::Memory(address) => Storage::Memory(
            address
                .checked_add(offset)
                .ok_or_else(|| PathError::at(expr, "address overflows"))?,
        ),
        Storage::Bytes(bytes, address) => {
            let bytes = usize::try_from(offset)
                .ok()
                .zip(offset.checked_add(size).and_then(|end| usize::try_from(end).ok()))
                .and_then(|(start, end)| bytes.get(start..end))
                .ok_or_else(|| PathError::at(expr, "value bytes missing"))?
                .to_vec();
            let address = match address {
                Some(address) => Some(
                    address
                        .checked_add(offset)
                        .ok_or_else(|| PathError::at(expr, "address overflows"))?,
                ),
                None => None,
            };
            Storage::Bytes(bytes, address)
        }
    };
    Ok(Place { ty, storage })
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn parsed(text: &str) -> String
    {
        Path::parse(text)
            .unwrap_or_else(|error| panic!("{text}: {error}"))
            .to_string()
    }

    fn rejected(text: &str) -> String
    {
        Path::parse(text).expect_err(text).to_string()
    }

    #[test]
    fn parses_fields_indexes_and_dereferences()
    {
        assert_eq!(parsed("p"), "p");
        assert_eq!(parsed("p.inner.x"), "p.inner.x");
        assert_eq!(parsed("v[2]"), "v[2]");
        assert_eq!(parsed("*boxed"), "*boxed");
        assert_eq!(parsed("t.1"), "t.1");
        assert_eq!(parsed("t.0.1"), "t.0.1");
        assert_eq!(parsed("(*r).name"), "(*r).name");
        assert_eq!(parsed(" ( * r ) . name "), "(*r).name");
        assert_eq!(parsed("**pp"), "**pp");
        assert_eq!(parsed("*a.b[3]"), "*a.b[3]");
        assert_eq!(parsed("((v))[0]"), "v[0]");
        assert_eq!(parsed("grid[1][2]"), "grid[1][2]");
        assert_eq!(parsed("λ_1"), "λ_1");
    }

    #[test]
    fn dereference_binds_looser_than_postfix_operators()
    {
        let path = Path::parse("*a.b").unwrap();
        assert!(matches!(&path.expr, Expr::Deref(inner) if matches!(**inner, Expr::Field(..))));
        let path = Path::parse("(*a).b").unwrap();
        assert!(matches!(&path.expr, Expr::Field(base, _) if matches!(**base, Expr::Deref(_))));
        assert_eq!(path.root(), "a");
        assert!(!path.is_variable());
        assert!(Path::parse("a").unwrap().is_variable());
    }

    #[test]
    fn rejects_function_calls()
    {
        for text in ["f()", "v.len()", "x.y(1)", "(*r).name()"] {
            assert!(
                rejected(text).contains("function calls are not supported"),
                "{text}: {}",
                rejected(text)
            );
        }
    }

    #[test]
    fn rejects_operators()
    {
        for text in ["a + b", "a-1", "!flag", "&x", "a == b", "v[1+1]", "-x", "a/2", "x?"] {
            let error = rejected(text);
            assert!(error.starts_with("unexpected `"), "{text}: {error}");
        }
        assert!(rejected("a + b").contains("operators are not supported"));
        assert!(rejected("&x").contains("operators are not supported"));
        assert_eq!(rejected("a + b"), "unexpected `+` at column 3: operators are not supported");
    }

    #[test]
    fn rejects_malformed_paths()
    {
        assert_eq!(rejected(""), "expected a variable name");
        assert_eq!(rejected("   "), "expected a variable name");
        assert_eq!(rejected("a."), "unexpected end of path");
        assert_eq!(rejected("a["), "unexpected end of path");
        assert_eq!(rejected("(a"), "unexpected end of path");
        assert_eq!(rejected("a b"), "unexpected `b` at column 3");
        assert_eq!(rejected("a)"), "unexpected `)` at column 2");
        assert_eq!(
            rejected("a[]"),
            "unexpected `]` at column 3: an index is a non-negative integer"
        );
        assert_eq!(
            rejected("v[i]"),
            "unexpected `i` at column 3: an index is a non-negative integer"
        );
        assert_eq!(
            rejected("0"),
            "unexpected `0` at column 1: a path starts with a variable name"
        );
        assert_eq!(rejected("a::b"), "unexpected `:` at column 2");
        assert_eq!(rejected("a.*"), "unexpected `*` at column 3");
        assert!(rejected("v[99999999999999999999999]").starts_with("invalid index"));
        assert_eq!(rejected("t.1x"), "invalid field index `1x` at column 3");
        assert_eq!(rejected("t.0x1"), "invalid field index `0x1` at column 3");
        assert!(rejected("1.0").contains("starts with a variable name"));
    }

    #[test]
    fn parses_index_literals_as_rust_does()
    {
        let index = |text: &str| match Path::parse(text).unwrap_or_else(|error| panic!("{text}: {error}")).expr {
            Expr::Index(_, index, _) => index,
            expr => panic!("{text}: {expr:?}"),
        };
        assert_eq!(index("v[1_000]"), 1000);
        assert_eq!(index("v[0x10]"), 16);
        assert_eq!(index("v[0xfF]"), 255);
        assert_eq!(index("v[0o17]"), 15);
        assert_eq!(index("v[0b101]"), 5);
        assert_eq!(index("v[0x_1]"), 1);
        assert_eq!(index("v[007]"), 7);
        assert_eq!(index("v[18446744073709551615]"), u64::MAX);
        // Errors name the segment as written.
        assert_eq!(parsed("v[0x10]"), "v[0x10]");
        assert_eq!(parsed("v[1_000]"), "v[1_000]");
        for text in ["v[0x]", "v[0b2]", "v[0o8]", "v[1a]", "v[0x_]", "v[0X1]", "v[1u8]"] {
            assert!(rejected(text).starts_with("invalid index"), "{text}: {}", rejected(text));
        }
        assert_eq!(rejected("v[0xg]"), "invalid index `0xg` at column 3");
        assert!(rejected("v[0x10000000000000000]").starts_with("invalid index"));
    }

    #[test]
    fn rejects_deeply_nested_paths()
    {
        let deep = format!("{}x", "*".repeat(100_000));
        assert_eq!(rejected(&deep), "path is longer than 256 tokens");
        let nested = format!("{}x{}", "(".repeat(100), ")".repeat(100));
        assert_eq!(parsed(&nested), "x");
    }

    #[test]
    fn errors_name_their_segment()
    {
        let path = Path::parse("(*r).items[4]").unwrap();
        let Expr::Index(base, ..) = &path.expr else {
            panic!("{path:?}");
        };
        assert_eq!(PathError::at(&path.expr, "out").to_string(), "`(*r).items[4]`: out");
        assert_eq!(PathError::at(base, "missing").to_string(), "`(*r).items`: missing");
    }
}
