//! Evaluate DWARF locations against the registers and memory of a stopped frame.

use std::collections::HashSet;

use gimli::{
    AttributeValue, Encoding, EvaluationResult, Expression, Location, Operation, RawLocListEntry, Reader as _,
    RunTimeEndian, Section as _, UnitRef, Value,
};

use crate::error::Result as DebuggerResult;
use crate::symbols::OwnedReader;
use crate::types::{Address, FrameRegisters};

/// Storage described by a variable's DWARF location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueLocation
{
    /// A contiguous range of memory beginning at this runtime address.
    Memory(Address),
    /// A register identified by its DWARF number.
    Register(u16),
    /// An explicitly sized piece or a composition of pieces in DWARF order.
    Pieces,
    /// Bytes supplied by DWARF itself, such as a constant.
    Implicit,
}

/// A variable's bytes in target memory order and the kind of location they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableValue
{
    /// Where the expression placed the value, if it could be determined.
    pub location: Option<ValueLocation>,
    /// Raw bytes, or why the complete value could not be recovered.
    pub bytes: std::result::Result<Vec<u8>, Unavailable>,
}

impl VariableValue
{
    pub(super) fn unavailable(reason: Unavailable) -> Self
    {
        Self {
            location: None,
            bytes: Err(reason),
        }
    }
}

/// Why a variable in scope could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable
{
    /// The variable has no location at this PC, or a piece is missing.
    #[error("optimized out")]
    OptimizedOut,
    /// The unwind did not recover the required DWARF register.
    #[error("register unknown (DWARF register {0})")]
    RegisterUnknown(u16),
    /// The unwind did not recover a canonical frame address.
    #[error("canonical frame address unknown")]
    CfaUnknown,
    /// The subprogram has no usable frame base at this PC.
    #[error("frame base unknown")]
    FrameBaseUnknown,
    /// DWARF does not give the object's byte size.
    #[error("value size unknown")]
    SizeUnknown,
    /// The value exceeds the maximum raw read size.
    #[error("value exceeds the {MAX_VALUE_BYTES}-byte read limit")]
    TooLarge,
    /// A location expression or piece requires functionality the evaluator lacks.
    #[error("unsupported DWARF location")]
    UnsupportedLocation,
    /// Malformed DWARF or a piece that does not fit its stated size.
    #[error("invalid DWARF location: {0}")]
    InvalidDwarf(#[from] gimli::Error),
    /// The variable's type or other metadata could not be read.
    #[error("invalid DWARF variable metadata: {0}")]
    InvalidMetadata(gimli::Error),
    /// A file address from DWARF could not be mapped into the loaded image.
    #[error("unmapped DWARF address 0x{0:x}")]
    UnmappedAddress(u64),
    /// The target memory could not supply all bytes of a value.
    #[error("{0}")]
    Memory(String),
}

// Bound allocations and target reads driven by sizes from debug information.
const MAX_VALUE_BYTES: usize = 1024 * 1024;

type EvalResult<T> = Result<T, Unavailable>;
pub(super) type LocationExpression = Expression<OwnedReader>;

/// Resolve a direct expression or the entry for `pc` in a location list. A DWARF 5
/// default entry applies only if none of the explicit ranges match.
pub(super) fn expression_at(
    unit: UnitRef<'_, OwnedReader>,
    attribute: Option<AttributeValue<OwnedReader>>,
    pc: u64,
) -> EvalResult<Option<LocationExpression>>
{
    let Some(attribute) = attribute else { return Ok(None) };
    if let Some(expression) = attribute.exprloc_value() {
        return Ok(Some(expression));
    }
    let mut list = unit.attr_locations(attribute)?.ok_or(Unavailable::UnsupportedLocation)?;
    let mut default = None;
    while let Some(raw) = list.next_raw()? {
        if let RawLocListEntry::DefaultLocation { data } = raw {
            default = Some(data);
        } else if let Some(entry) = list.convert_raw(raw)?
            && entry.range.begin <= pc
            && pc < entry.range.end
        {
            return Ok(Some(entry.data));
        }
    }
    Ok(default)
}

pub(super) struct LocationEvaluator<'a, Read, Relocate>
{
    unit: UnitRef<'a, OwnedReader>,
    registers: &'a FrameRegisters,
    read_memory: Read,
    relocate: Relocate,
    frame_base_expression: EvalResult<Option<LocationExpression>>,
    frame_base: Option<EvalResult<u64>>,
}

impl<'a, Read, Relocate> LocationEvaluator<'a, Read, Relocate>
where
    Read: FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
    Relocate: Fn(u64) -> Option<Address>,
{
    pub(super) fn new(
        unit: UnitRef<'a, OwnedReader>,
        registers: &'a FrameRegisters,
        read_memory: Read,
        relocate: Relocate,
        frame_base_expression: EvalResult<Option<LocationExpression>>,
    ) -> Self
    {
        Self {
            unit,
            registers,
            read_memory,
            relocate,
            frame_base_expression,
            frame_base: None,
        }
    }

    pub(super) fn read_value(&mut self, expression: LocationExpression, byte_size: Option<u64>) -> VariableValue
    {
        let pieces = match self.evaluate(expression, true) {
            Ok(pieces) => pieces,
            Err(reason) => return VariableValue::unavailable(reason),
        };
        if let [piece] = pieces.as_slice()
            && piece.size_in_bits.is_none()
        {
            let location = match piece.location {
                Location::Address { address } => ValueLocation::Memory(Address::new(address)),
                Location::Register { register } => ValueLocation::Register(register.0),
                Location::Value { .. } | Location::Bytes { .. } => ValueLocation::Implicit,
                Location::Empty => return VariableValue::unavailable(Unavailable::OptimizedOut),
                Location::ImplicitPointer { .. } => {
                    return VariableValue::unavailable(Unavailable::UnsupportedLocation);
                }
            };
            return VariableValue {
                location: Some(location),
                bytes: value_size(byte_size).and_then(|size| self.read_piece(piece, size)),
            };
        }
        if pieces.is_empty() {
            return VariableValue::unavailable(Unavailable::OptimizedOut);
        }
        VariableValue {
            location: Some(ValueLocation::Pieces),
            bytes: self.read_pieces(&pieces, byte_size),
        }
    }

    fn read_pieces(&mut self, pieces: &[gimli::Piece<OwnedReader>], byte_size: Option<u64>) -> EvalResult<Vec<u8>>
    {
        let size = byte_size.map(|size| value_size(Some(size))).transpose()?;
        let mut bytes = Vec::new();
        for piece in pieces {
            let bits = piece.size_in_bits.ok_or(gimli::Error::InvalidPiece)?;
            let len = byte_aligned(bits)?;
            if bytes.len() + len > MAX_VALUE_BYTES {
                return Err(Unavailable::TooLarge);
            }
            if size.is_some_and(|size| bytes.len() + len > size) {
                return Err(gimli::Error::InvalidPiece.into());
            }
            bytes.extend(self.read_piece(piece, len)?);
        }
        if size.is_some_and(|size| size != bytes.len()) {
            return Err(Unavailable::OptimizedOut);
        }
        Ok(bytes)
    }

    fn evaluate(
        &mut self,
        expression: LocationExpression,
        allow_frame_base: bool,
    ) -> EvalResult<Vec<gimli::Piece<OwnedReader>>>
    {
        // A zero-length location explicitly marks a value (or frame base) unavailable.
        if expression.0.is_empty() {
            return Err(if allow_frame_base {
                Unavailable::OptimizedOut
            } else {
                Unavailable::FrameBaseUnknown
            });
        }
        let encoding = self.unit.encoding();
        validate_piece_sizes(expression.0.clone(), encoding)?;
        let mut evaluation = expression.evaluation(encoding);
        evaluation.set_max_iterations(4096);
        let mut state = evaluation.evaluate()?;
        loop {
            state = match state {
                EvaluationResult::Complete => return Ok(evaluation.result()),
                EvaluationResult::RequiresRegister { register, base_type } if base_type.0 == 0 => {
                    evaluation.resume_with_register(Value::Generic(self.register(register.0)?))?
                }
                EvaluationResult::RequiresFrameBase if allow_frame_base => {
                    evaluation.resume_with_frame_base(self.frame_base()?)?
                }
                EvaluationResult::RequiresFrameBase => return Err(Unavailable::FrameBaseUnknown),
                EvaluationResult::RequiresCallFrameCfa => {
                    let cfa = self.registers.cfa().ok_or(Unavailable::CfaUnknown)?;
                    evaluation.resume_with_call_frame_cfa(cfa.value())?
                }
                EvaluationResult::RequiresRelocatedAddress(address) => {
                    evaluation.resume_with_relocated_address(self.runtime_address(address)?)?
                }
                EvaluationResult::RequiresIndexedAddress { index, relocate } => {
                    let address = self.unit.address(index)?;
                    let address = if relocate { self.runtime_address(address)? } else { address };
                    evaluation.resume_with_indexed_address(address)?
                }
                EvaluationResult::RequiresMemory {
                    address,
                    size,
                    space: None,
                    base_type,
                } if base_type.0 == 0 => {
                    let bytes = self.memory(Address::new(address), usize::from(size))?;
                    let bits = match self.endian() {
                        RunTimeEndian::Little => bytes.iter().rev().fold(0, |n, &byte| (n << 8) | u64::from(byte)),
                        RunTimeEndian::Big => bytes.iter().fold(0, |n, &byte| (n << 8) | u64::from(byte)),
                    };
                    evaluation.resume_with_memory(Value::Generic(bits))?
                }
                _ => return Err(Unavailable::UnsupportedLocation),
            };
        }
    }

    fn frame_base(&mut self) -> EvalResult<u64>
    {
        if let Some(base) = &self.frame_base {
            return base.clone();
        }
        let expression = self.frame_base_expression.clone()?.ok_or(Unavailable::FrameBaseUnknown)?;
        let base = self.evaluate_frame_base(expression);
        self.frame_base = Some(base.clone());
        base
    }

    fn evaluate_frame_base(&mut self, expression: LocationExpression) -> EvalResult<u64>
    {
        let pieces = self.evaluate(expression, false)?;
        let [piece] = pieces.as_slice() else {
            return Err(Unavailable::FrameBaseUnknown);
        };
        if piece.size_in_bits.is_some() {
            return Err(Unavailable::FrameBaseUnknown);
        }
        match piece.location {
            Location::Address { address } => Ok(address),
            Location::Register { register } => self.register(register.0),
            Location::Value { value } => Ok(value.to_u64(u64::MAX)?),
            _ => Err(Unavailable::FrameBaseUnknown),
        }
    }

    fn read_piece(&mut self, piece: &gimli::Piece<OwnedReader>, len: usize) -> EvalResult<Vec<u8>>
    {
        let offset = byte_aligned(piece.bit_offset.unwrap_or(0))?;
        match &piece.location {
            Location::Address { address } => {
                let address = address.checked_add(offset as u64).ok_or(gimli::Error::InvalidPiece)?;
                self.memory(Address::new(address), len)
            }
            Location::Register { register } => scalar_bytes(self.endian(), self.register(register.0)?, offset, len),
            Location::Value { value } => {
                let bits = match value {
                    Value::F32(value) => u64::from(value.to_bits()),
                    Value::F64(value) => value.to_bits(),
                    other => other.to_u64(u64::MAX)?,
                };
                scalar_bytes(self.endian(), bits, offset, len)
            }
            Location::Bytes { value } => reader_bytes(value, offset, len),
            Location::Empty => Err(Unavailable::OptimizedOut),
            Location::ImplicitPointer { .. } => Err(Unavailable::UnsupportedLocation),
        }
    }

    fn memory(&mut self, address: Address, len: usize) -> EvalResult<Vec<u8>>
    {
        if len == 0 {
            return Ok(Vec::new());
        }
        let bytes = (self.read_memory)(address, len).map_err(|error| Unavailable::Memory(error.to_string()))?;
        if bytes.len() != len {
            return Err(Unavailable::Memory(format!(
                "read {} of {len} bytes at {address}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    fn register(&self, reg: u16) -> EvalResult<u64>
    {
        self.registers.dwarf_register(reg).ok_or(Unavailable::RegisterUnknown(reg))
    }

    fn runtime_address(&self, address: u64) -> EvalResult<u64>
    {
        (self.relocate)(address)
            .map(Address::value)
            .ok_or(Unavailable::UnmappedAddress(address))
    }

    fn endian(&self) -> RunTimeEndian
    {
        unit_endian(self.unit)
    }
}

pub(super) fn read_constant(
    unit: UnitRef<'_, OwnedReader>,
    constant: gimli::Result<Option<AttributeValue<OwnedReader>>>,
    byte_size: Option<u64>,
) -> VariableValue
{
    let constant = match constant {
        Ok(Some(constant)) => constant,
        Ok(None) => return VariableValue::unavailable(Unavailable::OptimizedOut),
        Err(error) => return VariableValue::unavailable(Unavailable::InvalidMetadata(error)),
    };
    VariableValue {
        location: Some(ValueLocation::Implicit),
        bytes: value_size(byte_size).and_then(|size| constant_bytes(unit_endian(unit), constant, size)),
    }
}

fn constant_bytes(endian: RunTimeEndian, constant: AttributeValue<OwnedReader>, size: usize) -> EvalResult<Vec<u8>>
{
    match constant {
        AttributeValue::Block(bytes) => reader_bytes(&bytes, 0, size),
        other => {
            let bits = other
                .udata_value()
                .or_else(|| other.sdata_value().map(|value| value as u64))
                .ok_or(Unavailable::UnsupportedLocation)?;
            scalar_bytes(endian, bits, 0, size)
        }
    }
}

fn unit_endian(unit: UnitRef<'_, OwnedReader>) -> RunTimeEndian
{
    unit.dwarf.debug_info.reader().endian()
}

fn scalar_bytes(endian: RunTimeEndian, bits: u64, offset: usize, len: usize) -> EvalResult<Vec<u8>>
{
    let end = offset + len;
    if end > 8 {
        return Err(Unavailable::UnsupportedLocation);
    }
    Ok(match endian {
        RunTimeEndian::Little => bits.to_le_bytes()[offset..end].to_vec(),
        RunTimeEndian::Big => bits.to_be_bytes()[8 - end..8 - offset].to_vec(),
    })
}

fn value_size(size: Option<u64>) -> EvalResult<usize>
{
    let size = size.ok_or(Unavailable::SizeUnknown)?;
    if size > MAX_VALUE_BYTES as u64 {
        return Err(Unavailable::TooLarge);
    }
    Ok(size as usize)
}

/// gimli 0.32 multiplies a `DW_OP_piece` byte count by eight before checking it.
/// Follow branch targets as well as fallthrough so any piece the evaluator can
/// reach gets checked. Leave decode errors on each path to normal evaluation.
fn validate_piece_sizes(input: OwnedReader, encoding: Encoding) -> gimli::Result<()>
{
    let len = input.len();
    let mut pending = vec![0];
    let mut seen = HashSet::new();
    while let Some(start) = pending.pop() {
        let mut cursor = input.clone();
        if cursor.skip(start).is_err() {
            continue;
        }
        while !cursor.is_empty() && seen.insert(len - cursor.len()) {
            let mut probe = cursor.clone();
            if probe.read_u8() == Ok(gimli::DW_OP_piece.0)
                && probe.read_uleb128().is_ok_and(|size| size.checked_mul(8).is_none())
            {
                return Err(gimli::Error::InvalidPiece);
            }
            let Ok(op) = Operation::parse(&mut cursor, encoding) else {
                break;
            };
            if let Operation::Bra { target } | Operation::Skip { target } = op {
                let next = len - cursor.len();
                pending.push(next.wrapping_add_signed(isize::from(target)));
                if matches!(op, Operation::Skip { .. }) {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn byte_aligned(bits: u64) -> EvalResult<usize>
{
    if !bits.is_multiple_of(8) {
        return Err(Unavailable::UnsupportedLocation);
    }
    value_size(Some(bits / 8))
}

fn reader_bytes(reader: &OwnedReader, offset: usize, len: usize) -> EvalResult<Vec<u8>>
{
    let mut reader = reader.clone();
    reader.skip(offset)?;
    Ok(reader.split(len)?.to_slice()?.into_owned())
}

#[cfg(test)]
pub(super) mod tests
{
    use std::sync::Arc;

    use gimli::write::{DwarfUnit, EndianVec, Sections};
    use gimli::{Dwarf, Encoding, Format, constants as op};

    use super::*;
    use crate::symbols::OwnedDwarf;

    pub(in crate::values) fn encoding(version: u16) -> Encoding
    {
        Encoding {
            format: Format::Dwarf32,
            version,
            address_size: 8,
        }
    }

    pub(in crate::values) fn synthetic_dwarf(mut unit: DwarfUnit, endian: RunTimeEndian) -> OwnedDwarf
    {
        let mut sections = Sections::new(EndianVec::new(endian));
        unit.write(&mut sections).unwrap();
        Dwarf::load(|id| {
            let bytes = sections.get(id).map_or(&[][..], EndianVec::slice);
            Ok::<_, gimli::Error>(OwnedReader::new(Arc::from(bytes), endian))
        })
        .unwrap()
    }

    fn expr(data: &[u8], endian: RunTimeEndian) -> LocationExpression
    {
        Expression(OwnedReader::new(Arc::from(data), endian))
    }

    fn frame_registers() -> FrameRegisters
    {
        FrameRegisters::new(Some(Address::new(0x8030)), |reg| match reg {
            0 => Some(0x1234),
            29 => Some(0x8020),
            32 => Some(0x8040),
            _ => None,
        })
    }

    fn assert_read(value: VariableValue, location: ValueLocation, bytes: &[u8])
    {
        assert_eq!(value.location, Some(location));
        assert_eq!(value.bytes.as_deref(), Ok(bytes));
    }

    #[test]
    fn evaluates_frame_base_cfa_and_register_relative_addresses()
    {
        let endian = RunTimeEndian::Little;
        let dwarf = synthetic_dwarf(DwarfUnit::new(encoding(4)), endian);
        let unit = dwarf.unit(dwarf.units().next().unwrap().unwrap()).unwrap();
        let unit = UnitRef::new(&dwarf, &unit);
        let registers = frame_registers();
        let memory = |address: Address, len| Ok(vec![address.value() as u8; len]);
        let mut evaluator = LocationEvaluator::new(
            unit,
            &registers,
            memory,
            |_| None,
            Ok(Some(expr(&[op::DW_OP_reg29.0], endian))),
        );
        let cases: &[(&[u8], u64)] = &[
            (&[op::DW_OP_fbreg.0, 0x78], 0x8018),     // x29 - 8
            (&[op::DW_OP_breg29.0, 0x04], 0x8024),    // x29 + 4
            (&[op::DW_OP_bregx.0, 32, 0x7c], 0x803c), // PC - 4
            (&[op::DW_OP_call_frame_cfa.0], 0x8030),
        ];
        for &(ops, address) in cases {
            assert_read(
                evaluator.read_value(expr(ops, endian), Some(2)),
                ValueLocation::Memory(Address::new(address)),
                &[address as u8; 2],
            );
        }
        let mut evaluator = LocationEvaluator::new(
            unit,
            &registers,
            memory,
            |_| None,
            Ok(Some(expr(&[op::DW_OP_call_frame_cfa.0], endian))),
        );
        assert_read(
            evaluator.read_value(expr(&[op::DW_OP_fbreg.0, 0x78], endian), Some(2)),
            ValueLocation::Memory(Address::new(0x8028)),
            &[0x28; 2],
        );
    }

    #[test]
    fn assembles_register_memory_and_implicit_pieces_in_target_order()
    {
        for endian in [RunTimeEndian::Little, RunTimeEndian::Big] {
            let dwarf = synthetic_dwarf(DwarfUnit::new(encoding(5)), endian);
            let unit = dwarf.unit(dwarf.units().next().unwrap().unwrap()).unwrap();
            let registers = frame_registers();
            let mut evaluator = LocationEvaluator::new(
                UnitRef::new(&dwarf, &unit),
                &registers,
                |address, len| {
                    assert_eq!((address, len), (Address::new(0x8024), 2));
                    Ok(vec![0xab, 0xcd])
                },
                |_| None,
                Ok(None),
            );
            let register_bytes = match endian {
                RunTimeEndian::Little => [0x34, 0x12],
                RunTimeEndian::Big => [0x12, 0x34],
            };
            assert_read(
                evaluator.read_value(expr(&[op::DW_OP_reg0.0], endian), Some(2)),
                ValueLocation::Register(0),
                &register_bytes,
            );
            assert_read(
                evaluator.read_value(expr(&[op::DW_OP_regx.0, 32], endian), Some(1)),
                ValueLocation::Register(32),
                &[0x40],
            );
            let implicit = [op::DW_OP_lit7.0, op::DW_OP_stack_value.0];
            assert_read(
                evaluator.read_value(expr(&implicit, endian), Some(1)),
                ValueLocation::Implicit,
                &[7],
            );
            let pieces = [
                op::DW_OP_reg0.0,
                op::DW_OP_piece.0,
                2,
                op::DW_OP_breg29.0,
                4,
                op::DW_OP_piece.0,
                2,
                op::DW_OP_implicit_value.0,
                2,
                0xef,
                0x01,
                op::DW_OP_piece.0,
                2,
            ];
            assert_read(
                evaluator.read_value(expr(&pieces, endian), Some(6)),
                ValueLocation::Pieces,
                &[register_bytes[0], register_bytes[1], 0xab, 0xcd, 0xef, 0x01],
            );
            let missing_piece = [op::DW_OP_reg0.0, op::DW_OP_piece.0, 2, op::DW_OP_piece.0, 2];
            let missing = evaluator.read_value(expr(&missing_piece, endian), Some(4));
            assert_eq!(missing.location, Some(ValueLocation::Pieces));
            assert_eq!(missing.bytes, Err(Unavailable::OptimizedOut));
        }
    }

    #[test]
    fn reports_unavailable_inputs_and_preserves_known_locations()
    {
        let endian = RunTimeEndian::Little;
        let dwarf = synthetic_dwarf(DwarfUnit::new(encoding(4)), endian);
        let unit = dwarf.unit(dwarf.units().next().unwrap().unwrap()).unwrap();
        let registers = FrameRegisters::new(None, |_| None);
        let mut evaluator = LocationEvaluator::new(
            UnitRef::new(&dwarf, &unit),
            &registers,
            |_, _| panic!("these expressions must not read memory"),
            |file| Some(Address::new(file + 0x1000)),
            Ok(None),
        );
        let reg = evaluator.read_value(expr(&[op::DW_OP_reg0.0], endian), Some(4));
        assert_eq!(reg.location, Some(ValueLocation::Register(0)));
        assert_eq!(reg.bytes, Err(Unavailable::RegisterUnknown(0)));
        for (ops, expected) in [
            (&[op::DW_OP_breg0.0, 0][..], Unavailable::RegisterUnknown(0)),
            (&[op::DW_OP_call_frame_cfa.0][..], Unavailable::CfaUnknown),
            (&[op::DW_OP_fbreg.0, 0][..], Unavailable::FrameBaseUnknown),
            (&[][..], Unavailable::OptimizedOut),
        ] {
            let value = evaluator.read_value(expr(ops, endian), Some(4));
            assert_eq!(value.location, None);
            assert_eq!(value.bytes, Err(expected));
        }
        let ops = [op::DW_OP_addr.0, 0x20, 0x40, 0, 0, 0, 0, 0, 0];
        let value = evaluator.read_value(expr(&ops, endian), None);
        assert_eq!(value.location, Some(ValueLocation::Memory(Address::new(0x5020))));
        assert_eq!(value.bytes, Err(Unavailable::SizeUnknown));
    }

    #[test]
    fn rejects_piece_size_that_cannot_be_represented_in_bits()
    {
        use gimli::write::{self, Writer as _};

        let endian = RunTimeEndian::Little;
        let dwarf = synthetic_dwarf(DwarfUnit::new(encoding(4)), endian);
        let unit = dwarf.unit(dwarf.units().next().unwrap().unwrap()).unwrap();
        let registers = frame_registers();
        let mut evaluator = LocationEvaluator::new(
            UnitRef::new(&dwarf, &unit),
            &registers,
            |_, _| panic!("malformed location must not read memory"),
            |_| None,
            Ok(None),
        );
        let mut ops = write::EndianVec::new(endian);
        ops.write_u8(op::DW_OP_reg0.0).unwrap();
        ops.write_u8(op::DW_OP_piece.0).unwrap();
        ops.write_uleb128(u64::MAX / 8 + 1).unwrap();
        let value = evaluator.read_value(expr(ops.slice(), endian), Some(1));
        assert_eq!(value.bytes, Err(Unavailable::InvalidDwarf(gimli::Error::InvalidPiece)));
    }
}
