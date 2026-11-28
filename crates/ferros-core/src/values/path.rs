//! Parse and evaluate `print <path>`: a variable followed by field access, indexing and
//! dereference.
//!
//! The grammar, loosest first:
//!
//! ```text
//! path    := '*' path | postfix
//! postfix := primary ( '.' name | '.' integer | '[' integer ']' )*
//! primary := name | '(' path ')'
//! name    := ( '::'? identifier | qualifier '::' identifier ) ( '::' identifier )*
//! qualifier := '<' any text with balanced '<' '>' '>'
//! ```
//!
//! A name with `::` segments is a static's qualified name (`my_crate::config::LIMIT`). A
//! static in an impl method starts with the impl's qualifier, written as `globals` shows it:
//! `<my_crate::Meters>::f::X`, `<my_crate::Meters as my_crate::Unit>::f::X`.
//!
//! As in Rust, `*a.b` dereferences `a.b`; write `(*a).b` to dereference `a` first. `.0`
//! selects a tuple or tuple-struct field. On a Rust enum, `.field` and `.0` select a field
//! of the active variant, read from the discriminant (an explicit tag or a niche) as the
//! enum printer does; a field only another variant has is an error naming both. `[n]`
//! indexes an array, a slice (`&[T]`, `Box<[T]>`) or a `Vec`. `*` follows a reference, a
//! raw pointer or a `Box`. Field access and indexing look through references and boxes as
//! Rust's auto-dereference does; raw pointers need an explicit `*`. There are no function
//! calls, operators or literals other than an index.
//!
//! Evaluation reads only what the path needs: the root variable's bytes, the pointers and
//! headers it follows, and the bytes of the final value, which the caller formats.

use std::fmt;

use gimli::RunTimeEndian;

use super::format::{ValueView, unsigned};
use super::location::value_size;
use super::pretty::{fat_pointer, variant_for_tag, variant_payload, vec_layout};
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
    Index(Box<Expr>, u64),
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
                Expr::Field(base, _) | Expr::Index(base, _) | Expr::Deref(base) => expr = base,
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
            Self::Index(base, index) => {
                write_base(f, base)?;
                write!(f, "[{index}]")
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
            ch if ch.is_ascii_digit() => {
                let end = word_end(text, start);
                while chars.next_if(|&(next, _)| next < end).is_some() {}
                Token::Integer(&text[start..end])
            }
            ch if is_name_start(ch) || ch == '<' || (ch == ':' && text[start..].starts_with("::")) => {
                match name_end(text, start) {
                    Some(end) => {
                        while chars.next_if(|&(next, _)| next < end).is_some() {}
                        Token::Name(&text[start..end])
                    }
                    None => Token::Other(ch),
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

fn is_name_start(ch: char) -> bool
{
    ch == '_' || ch.is_alphabetic()
}

/// The end of the run of identifier characters at `start`.
fn word_end(text: &str, start: usize) -> usize
{
    text[start..]
        .char_indices()
        .find(|&(_, ch)| !(ch.is_alphanumeric() || ch == '_'))
        .map_or(text.len(), |(offset, _)| start + offset)
}

/// The end of the name at `start`: identifiers joined by `::`, optionally after a leading
/// `::` or an impl qualifier, as in a static's qualified name (`::my_crate::config::LIMIT`,
/// `<my_crate::Meters as my_crate::Unit>::f::X`). `None` when `::` at `start` is not
/// followed by an identifier, or a `<` at `start` does not close and continue with `::`
/// and an identifier.
fn name_end(text: &str, start: usize) -> Option<usize>
{
    let segment = |at: usize| {
        let rest = text.get(at..)?;
        rest.starts_with(is_name_start).then(|| word_end(text, at))
    };
    let mut end = if text[start..].starts_with("::") {
        segment(start + 2)?
    } else if text[start..].starts_with('<') {
        let close = qualifier_end(text, start)?;
        text[close..].starts_with("::").then_some(())?;
        segment(close + 2)?
    } else {
        segment(start)?
    };
    while text[end..].starts_with("::")
        && let Some(next) = segment(end + 2)
    {
        end = next;
    }
    Some(end)
}

/// The end of the `<...>` at `start`, just after the `>` that balances its `<`.
fn qualifier_end(text: &str, start: usize) -> Option<usize>
{
    let mut depth = 0_usize;
    for (offset, ch) in text[start..].char_indices() {
        match ch {
            '<' => depth += 1,
            // Not the `>` of a return type's `->`, as in `<fn() -> u8 as app::Unit>`.
            '>' if !text[..start + offset].ends_with('-') => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
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
                        // `::` belongs only to the root: a field name is one identifier.
                        (Token::Name(name), column) if name.contains("::") => {
                            return Err(PathError::syntax(format!(
                                "unexpected `{name}` at column {column}: a field name has no `::`"
                            )));
                        }
                        (Token::Name(name), _) => name.to_owned(),
                        (Token::Integer(index), column) => {
                            parse_index(index, column)?;
                            index.to_owned()
                        }
                        (token, column) => return Err(unexpected(token, column)),
                    };
                    expr = Expr::Field(Box::new(expr), name);
                }
                Some(Token::OpenBracket) => {
                    self.next += 1;
                    let index = match self.take()? {
                        (Token::Integer(index), column) => parse_index(index, column)?,
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
                    expr = Expr::Index(Box::new(expr), index);
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

fn parse_index(text: &str, column: usize) -> Result<u64, PathError>
{
    text.parse()
        .map_err(|_| PathError::syntax(format!("invalid index `{text}` at column {column}")))
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
    Evaluator { types, endian, read }.eval(&path.expr, root)
}

struct Evaluator<'a>
{
    types: &'a TypeTable,
    endian: RunTimeEndian,
    read: &'a mut dyn FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
}

/// What a value is to the path operations, after typedefs and qualifiers.
enum Shape
{
    Members,
    Array(TypeId, u64),
    Elements,
    Pointer(PointerKind, Option<TypeId>),
    Other,
}

impl Evaluator<'_>
{
    fn eval(&mut self, expr: &Expr, root: Place) -> Result<Place, PathError>
    {
        match expr {
            Expr::Variable(_) => Ok(root),
            Expr::Field(base, name) => {
                let place = self.eval(base, root)?;
                let place = self.auto_deref(base, place)?;
                self.field(expr, place, name)
            }
            Expr::Index(base, index) => {
                let place = self.eval(base, root)?;
                let place = self.auto_deref(base, place)?;
                self.index(expr, place, *index)
            }
            Expr::Deref(inner) => {
                let place = self.eval(inner, root)?;
                self.deref(expr, place)
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
            TypeKind::Struct(_)
                if name.starts_with("alloc::vec::Vec<")
                    || name.starts_with("&[")
                    || name.starts_with("&mut [")
                    || name
                        .strip_prefix("alloc::boxed::Box<")
                        .is_some_and(|boxed| boxed.starts_with('[')) =>
            {
                Shape::Elements
            }
            TypeKind::Struct(_) | TypeKind::Tuple(_) | TypeKind::Union(_) if !ty.declaration => Shape::Members,
            _ => Shape::Other,
        }
    }

    fn auto_deref(&mut self, expr: &Expr, mut place: Place) -> Result<Place, PathError>
    {
        // Bound the walk so a malformed self-referential pointer type cannot loop.
        for _ in 0..16 {
            match self.shape(place.ty) {
                Shape::Pointer(PointerKind::Reference { .. } | PointerKind::Box, Some(_)) => {
                    place = self.deref(expr, place)?;
                }
                _ => break,
            }
        }
        Ok(place)
    }

    fn field(&mut self, expr: &Expr, place: Place, name: &str) -> Result<Place, PathError>
    {
        let ty = self.types.strip_aliases(place.ty);
        let fields = match self.types.get(ty).map(|ty| ty.kind.clone()) {
            Some(TypeKind::Struct(fields) | TypeKind::Tuple(fields) | TypeKind::Union(fields))
                if matches!(self.shape(ty), Shape::Members | Shape::Elements) =>
            {
                fields
            }
            Some(TypeKind::Enum(layout)) => return self.variant_field(expr, place, ty, &layout, name),
            _ => {
                let hint = raw_pointer_hint(expr, &self.shape(ty));
                return Err(PathError::at(expr, format!("{} has no fields{hint}", self.type_name(ty))));
            }
        };
        let member = member_named(&fields.members, name)
            .ok_or_else(|| PathError::at(expr, format!("no field `{name}` in {}", self.type_name(ty))))?;
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

    fn index(&mut self, expr: &Expr, place: Place, index: u64) -> Result<Place, PathError>
    {
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
                let bytes = self.load(expr, &place)?;
                let ty = self.types.strip_aliases(place.ty);
                let view = ValueView {
                    ty,
                    bytes: &bytes,
                    address: None,
                };
                let layout = vec_layout(view, self.types, self.endian)
                    .map(|vec| (vec.address, vec.len, vec.element))
                    .or_else(|| fat_pointer(view, self.types, self.endian));
                let Some((data, len, element)) = layout else {
                    return Err(PathError::at(
                        expr,
                        format!("unrecognized layout of {}", self.type_name(place.ty)),
                    ));
                };
                check_bounds(expr, index, len)?;
                if data == Address::ZERO {
                    return Err(PathError::at(expr, "null element data pointer"));
                }
                let size = self.element_size(expr, element)?;
                let address = index
                    .checked_mul(size)
                    .and_then(|offset| data.checked_add(offset))
                    .ok_or_else(|| PathError::at(expr, "element address overflows"))?;
                Ok(Place {
                    ty: element,
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

    fn deref(&mut self, expr: &Expr, place: Place) -> Result<Place, PathError>
    {
        let (kind, pointee) = match self.shape(place.ty) {
            Shape::Pointer(kind @ (PointerKind::Reference { .. } | PointerKind::Raw { .. } | PointerKind::Box), pointee) => {
                (kind, pointee)
            }
            Shape::Elements => {
                return Err(PathError::at(
                    expr,
                    format!(
                        "cannot dereference {} to an unsized value; print it whole or index it",
                        self.type_name(place.ty)
                    ),
                ));
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
        Ok(Place {
            ty: pointee,
            storage: Storage::Memory(Address::new(address)),
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

/// Raw pointers are not dereferenced implicitly. Suggest the explicit form.
fn raw_pointer_hint(expr: &Expr, shape: &Shape) -> String
{
    if !matches!(shape, Shape::Pointer(PointerKind::Raw { .. }, _)) {
        return String::new();
    }
    let explicit = match expr {
        Expr::Field(base, name) => Expr::Field(Box::new(Expr::Deref(base.clone())), name.clone()),
        Expr::Index(base, index) => Expr::Index(Box::new(Expr::Deref(base.clone())), *index),
        _ => return String::new(),
    };
    format!("; dereference the raw pointer first, as in `{explicit}`")
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
    fn a_qualified_name_is_one_root()
    {
        let path = Path::parse("app::config::LIMIT").unwrap();
        assert!(path.is_variable());
        assert_eq!(path.root(), "app::config::LIMIT");
        let path = Path::parse("app::TABLE[2]").unwrap();
        assert_eq!(path.root(), "app::TABLE");
        assert_eq!(path.to_string(), "app::TABLE[2]");
        assert_eq!(Path::parse("(*app::P).x").unwrap().root(), "app::P");
        assert_eq!(Path::parse("::app::LIMIT").unwrap().root(), "::app::LIMIT");
        let path = Path::parse("<app::Meters as app::Unit>::f::X.0").unwrap();
        assert_eq!(path.root(), "<app::Meters as app::Unit>::f::X");
        assert_eq!(path.to_string(), "<app::Meters as app::Unit>::f::X.0");
        assert_eq!(
            Path::parse("*<alloc::vec::Vec<_> as app::Unit>::f::P").unwrap().root(),
            "<alloc::vec::Vec<_> as app::Unit>::f::P"
        );
        assert_eq!(
            Path::parse("<fn() -> u8 as app::Unit>::f::X").unwrap().root(),
            "<fn() -> u8 as app::Unit>::f::X"
        );
        assert_eq!(
            rejected("<app::Meters>"),
            "unexpected `<` at column 1: operators are not supported"
        );
        assert_eq!(
            rejected("<app::Meters::X"),
            "unexpected `<` at column 1: operators are not supported"
        );
        assert_eq!(rejected("<a>::1"), "unexpected `<` at column 1: operators are not supported");
        assert_eq!(rejected("a < b"), "unexpected `<` at column 3: operators are not supported");
        assert_eq!(rejected("::"), "unexpected `:` at column 1");
        assert_eq!(rejected("app::"), "unexpected `:` at column 4");
        assert_eq!(rejected("app::1"), "unexpected `:` at column 4");
        assert_eq!(rejected("a:b"), "unexpected `:` at column 2");
        assert_eq!(rejected("s.a::b"), "unexpected `a::b` at column 3: a field name has no `::`");
        assert_eq!(
            rejected("app::S.x::y"),
            "unexpected `x::y` at column 8: a field name has no `::`"
        );
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
        assert_eq!(rejected("a: :b"), "unexpected `:` at column 2");
        assert_eq!(rejected("a.*"), "unexpected `*` at column 3");
        assert!(rejected("v[99999999999999999999999]").starts_with("invalid index"));
        assert!(rejected("t.1x").starts_with("invalid index"));
        assert!(rejected("1.0").contains("starts with a variable name"));
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
        let Expr::Index(base, _) = &path.expr else {
            panic!("{path:?}");
        };
        assert_eq!(PathError::at(&path.expr, "out").to_string(), "`(*r).items[4]`: out");
        assert_eq!(PathError::at(base, "missing").to_string(), "`(*r).items`: missing");
    }
}
