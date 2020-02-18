//! Lowering rules for ARM64.

#![allow(dead_code)]

use crate::ir::condcodes::IntCC;
use crate::ir::types::*;
use crate::ir::Inst as IRInst;
use crate::ir::{Block, InstructionData, Opcode, Type};
use crate::machinst::lower::*;
use crate::machinst::*;

use crate::isa::arm64::abi::*;
use crate::isa::arm64::inst::*;
use crate::isa::arm64::Arm64Backend;

use regalloc::{RealReg, Reg, RegClass, VirtualReg, Writable};

use smallvec::SmallVec;

//============================================================================
// Helpers: opcode conversions

fn op_to_aluop(op: Opcode, ty: Type) -> Option<ALUOp> {
    match (op, ty) {
        (Opcode::Iadd, I32) => Some(ALUOp::Add32),
        (Opcode::Iadd, I64) => Some(ALUOp::Add64),
        (Opcode::Isub, I32) => Some(ALUOp::Sub32),
        (Opcode::Isub, I64) => Some(ALUOp::Sub64),
        _ => None,
    }
}

fn is_alu_op(op: Opcode, ctrl_typevar: Type) -> bool {
    op_to_aluop(op, ctrl_typevar).is_some()
}

//============================================================================
// Result enum types.
//
// Lowering of a given value results in one of these enums, depending on the
// modes in which we can accept the value.

/// A lowering result: register, register-shift, register-extend.  An SSA value can always be
/// lowered into one of these options; the register form is the fallback.
#[derive(Clone, Debug)]
enum ResultRSE {
    Reg(Reg),
    RegShift(Reg, ShiftOpAndAmt),
    RegExtend(Reg, ExtendOp),
}

/// A lowering result: register, register-shift, register-extend, or 12-bit immediate form.
/// An SSA value can always be lowered into one of these options; the register form is the
/// fallback.
#[derive(Clone, Debug)]
enum ResultRSEImm12 {
    Reg(Reg),
    RegShift(Reg, ShiftOpAndAmt),
    RegExtend(Reg, ExtendOp),
    Imm12(Imm12),
}

impl ResultRSEImm12 {
    fn from_rse(rse: ResultRSE) -> ResultRSEImm12 {
        match rse {
            ResultRSE::Reg(r) => ResultRSEImm12::Reg(r),
            ResultRSE::RegShift(r, s) => ResultRSEImm12::RegShift(r, s),
            ResultRSE::RegExtend(r, e) => ResultRSEImm12::RegExtend(r, e),
        }
    }
}

/// A lowering result: register, register-shift, register-extend, or logical immediate form.
/// An SSA value can always be lowered into one of these options; the register form is the
/// fallback.
#[derive(Clone, Debug)]
enum ResultRSEImmLogic {
    Reg(Reg),
    RegShift(Reg, ShiftOpAndAmt),
    RegExtend(Reg, ExtendOp),
    ImmLogic(ImmLogic),
}

impl ResultRSEImmLogic {
    fn from_rse(rse: ResultRSE) -> ResultRSEImmLogic {
        match rse {
            ResultRSE::Reg(r) => ResultRSEImmLogic::Reg(r),
            ResultRSE::RegShift(r, s) => ResultRSEImmLogic::RegShift(r, s),
            ResultRSE::RegExtend(r, e) => ResultRSEImmLogic::RegExtend(r, e),
        }
    }
}

//============================================================================
// Instruction input and output "slots".
//
// We use these types to refer to operand numbers, and result numbers, together
// with the associated instruction, in a type-safe way.

/// Identifier for a particular output of an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InsnOutput {
    insn: IRInst,
    output: usize,
}

/// Identifier for a particular input of an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InsnInput {
    insn: IRInst,
    input: usize,
}

/// Producer of a value: either a previous instruction's output, or a register that will be
/// codegen'd separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsnInputSource {
    Output(InsnOutput),
    Reg(Reg),
}

impl InsnInputSource {
    fn as_output(self) -> Option<InsnOutput> {
        match self {
            InsnInputSource::Output(o) => Some(o),
            _ => None,
        }
    }
}

fn get_input<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, output: InsnOutput, num: usize) -> InsnInput {
    assert!(num <= ctx.num_inputs(output.insn));
    InsnInput {
        insn: output.insn,
        input: num,
    }
}

/// Convert an instruction input to a producing instruction's output if possible (in same BB), or a
/// register otherwise.
fn input_source<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, input: InsnInput) -> InsnInputSource {
    if let Some((input_inst, result_num)) = ctx.input_inst(input.insn, input.input) {
        let out = InsnOutput {
            insn: input_inst,
            output: result_num,
        };
        InsnInputSource::Output(out)
    } else {
        let reg = ctx.input(input.insn, input.input);
        InsnInputSource::Reg(reg)
    }
}

//============================================================================
// Lowering: convert instruction outputs to result types.

/// Lower an instruction output to a 64-bit constant, if possible.
fn output_to_const<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, out: InsnOutput) -> Option<u64> {
    if out.output > 0 {
        None
    } else {
        let inst_data = ctx.data(out.insn);
        if inst_data.opcode() == Opcode::Null {
            Some(0)
        } else {
            match inst_data {
                &InstructionData::UnaryImm { opcode: _, imm } => {
                    // Only has Into for i64; we use u64 elsewhere, so we cast.
                    let imm: i64 = imm.into();
                    Some(imm as u64)
                }
                &InstructionData::UnaryIeee32 { opcode: _, imm } => Some(imm.bits() as u64),
                &InstructionData::UnaryIeee64 { opcode: _, imm } => Some(imm.bits()),
                _ => None,
            }
        }
    }
}

/// Lower an instruction output to a constant register-shift amount, if possible.
fn output_to_shiftimm<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    out: InsnOutput,
) -> Option<ShiftOpShiftImm> {
    output_to_const(ctx, out).and_then(ShiftOpShiftImm::maybe_from_shift)
}

/// Lower an instruction input to a reg.
///
/// The given register will be extended appropriately, according to
/// `narrow_mode` and the input's type. If the input's type is <= 32 bits, the
/// value will be extended (if specified) into 32 bits; otherwise, 64 bits.
fn input_to_reg<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> Reg {
    let ty = ctx.input_ty(input.insn, input.input);
    let from_bits = ty_bits(ty) as u8;
    let to_bits = if from_bits < 32 { 32 } else { 64 } as u8;
    let raw_reg = ctx.input(input.insn, input.input);
    match (narrow_mode, from_bits) {
        (NarrowValueMode::None, _) => raw_reg,
        (NarrowValueMode::ZeroExtend, n) if n < 32 => {
            ctx.emit(Inst::Extend {
                rd: Writable::from_reg(raw_reg),
                rn: raw_reg,
                signed: false,
                from_bits,
                to_bits,
            });
            raw_reg
        }
        (NarrowValueMode::SignExtend, n) if n < 32 => {
            ctx.emit(Inst::Extend {
                rd: Writable::from_reg(raw_reg),
                rn: raw_reg,
                signed: true,
                from_bits,
                to_bits,
            });
            raw_reg
        }
        _ => raw_reg,
    }
}

/// Lower an instruction output to a reg.
fn output_to_reg<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, out: InsnOutput) -> Writable<Reg> {
    ctx.output(out.insn, out.output)
}

/// How to handle narrow values loaded into registers; see note on `narrow_mode`
/// parameter to `output_to_rse` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NarrowValueMode {
    None,
    ZeroExtend,
    SignExtend,
}

/// Lower an instruction output to a reg, reg/shift, or reg/extend operand.
/// This does not actually codegen the source instruction; it just uses the
/// vreg into which the source instruction will generate its value.
///
/// The `narrow_mode` flag indicates whether the consumer of this value needs
/// the high bits clear. For many operations, such as an add/sub/mul or any
/// bitwise logical operation, the low-bit results depend only on the low-bit
/// inputs, so e.g. we can do an 8 bit add on 32 bit registers where the 8-bit
/// value is stored in the low 8 bits of the register and the high 24 bits are
/// undefined. If the op truly needs the high N bits clear (such as for a
/// divide or a right-shift or a compare-to-zero), `narrow_mode` should be
/// set to `ZeroExtend` or `SignExtend` as appropriate, and the resulting
/// register will be provided the extended value.
fn output_to_rse<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    out: InsnOutput,
    narrow_mode: NarrowValueMode,
) -> ResultRSE {
    let insn = out.insn;
    assert!(out.output <= ctx.num_outputs(insn));
    let op = ctx.data(insn).opcode();
    let out_ty = ctx.output_ty(insn, out.output);
    let out_bits = ty_bits(out_ty);

    // If `out_ty` is smaller than 32 bits and we need to zero- or sign-extend,
    // then get the result into a register and return an Extend-mode operand on
    // that register.
    if out_bits < 32 && narrow_mode != NarrowValueMode::None {
        let reg = output_to_reg(ctx, out);
        let extendop = match (narrow_mode, out_bits) {
            (NarrowValueMode::SignExtend, 1) => ExtendOp::SXTB,
            (NarrowValueMode::ZeroExtend, 1) => ExtendOp::UXTB,
            (NarrowValueMode::SignExtend, 8) => ExtendOp::SXTB,
            (NarrowValueMode::ZeroExtend, 8) => ExtendOp::UXTB,
            (NarrowValueMode::SignExtend, 16) => ExtendOp::SXTH,
            (NarrowValueMode::ZeroExtend, 16) => ExtendOp::UXTH,
            _ => unreachable!(),
        };
        return ResultRSE::RegExtend(reg.to_reg(), extendop);
    }

    if op == Opcode::Ishl {
        let shiftee = get_input(ctx, out, 0);
        let shift_amt = get_input(ctx, out, 1);

        // Can we get the shift amount as an immediate?
        if let Some(shift_amt_out) = input_source(ctx, shift_amt).as_output() {
            if let Some(shiftimm) = output_to_shiftimm(ctx, shift_amt_out) {
                let reg = input_to_reg(ctx, shiftee, narrow_mode);
                ctx.merged(insn);
                ctx.merged(shift_amt_out.insn);
                return ResultRSE::RegShift(reg, ShiftOpAndAmt::new(ShiftOp::LSL, shiftimm));
            }
        }
    }

    // Is this a zero-extend or sign-extend and can we handle that with a register-mode operator?
    if op == Opcode::Uextend || op == Opcode::Sextend {
        assert!(out_bits == 32 || out_bits == 64);
        let sign_extend = op == Opcode::Sextend;
        let extendee = get_input(ctx, out, 0);
        let inner_ty = ctx.input_ty(extendee.insn, extendee.input);
        let inner_bits = ty_bits(inner_ty);
        assert!(inner_bits < out_bits);
        let extendop = match (sign_extend, inner_bits) {
            (true, 1) => ExtendOp::SXTB,
            (false, 1) => ExtendOp::UXTB,
            (true, 8) => ExtendOp::SXTB,
            (false, 8) => ExtendOp::UXTB,
            (true, 16) => ExtendOp::SXTH,
            (false, 16) => ExtendOp::UXTH,
            (true, 32) => ExtendOp::SXTW,
            (false, 32) => ExtendOp::UXTW,
            _ => unreachable!(),
        };
        let reg = input_to_reg(ctx, extendee, NarrowValueMode::None);
        ctx.merged(insn);
        return ResultRSE::RegExtend(reg, extendop);
    }

    // Otherwise, just return the register corresponding to the output.
    ResultRSE::Reg(output_to_reg(ctx, out).to_reg())
}

/// Lower an instruction output to a reg, reg/shift, reg/extend, or 12-bit immediate operand.
fn output_to_rse_imm12<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    out: InsnOutput,
    narrow_mode: NarrowValueMode,
) -> ResultRSEImm12 {
    if let Some(imm_value) = output_to_const(ctx, out) {
        if let Some(i) = Imm12::maybe_from_u64(imm_value) {
            ctx.merged(out.insn);
            return ResultRSEImm12::Imm12(i);
        }
    }

    ResultRSEImm12::from_rse(output_to_rse(ctx, out, narrow_mode))
}

/// Lower an instruction output to a reg, reg/shift, reg/extend, or logic-immediate operand.
fn output_to_rse_immlogic<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    out: InsnOutput,
    narrow_mode: NarrowValueMode,
) -> ResultRSEImmLogic {
    if let Some(imm_value) = output_to_const(ctx, out) {
        if let Some(i) = ImmLogic::maybe_from_u64(imm_value) {
            ctx.merged(out.insn);
            return ResultRSEImmLogic::ImmLogic(i);
        }
    }

    ResultRSEImmLogic::from_rse(output_to_rse(ctx, out, narrow_mode))
}

fn input_to_rse<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> ResultRSE {
    match input_source(ctx, input) {
        InsnInputSource::Output(out) => output_to_rse(ctx, out, narrow_mode),
        InsnInputSource::Reg(reg) => ResultRSE::Reg(reg),
    }
}

fn input_to_rse_imm12<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> ResultRSEImm12 {
    match input_source(ctx, input) {
        InsnInputSource::Output(out) => output_to_rse_imm12(ctx, out, narrow_mode),
        InsnInputSource::Reg(reg) => ResultRSEImm12::Reg(reg),
    }
}

fn input_to_rse_immlogic<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> ResultRSEImmLogic {
    match input_source(ctx, input) {
        InsnInputSource::Output(out) => output_to_rse_immlogic(ctx, out, narrow_mode),
        InsnInputSource::Reg(reg) => ResultRSEImmLogic::Reg(reg),
    }
}

fn alu_inst_imm12(op: ALUOp, rd: Writable<Reg>, rn: Reg, rm: ResultRSEImm12) -> Inst {
    match rm {
        ResultRSEImm12::Imm12(imm12) => Inst::AluRRImm12 {
            alu_op: op,
            rd,
            rn,
            imm12,
        },
        ResultRSEImm12::Reg(rm) => Inst::AluRRR {
            alu_op: op,
            rd,
            rn,
            rm,
        },
        ResultRSEImm12::RegShift(rm, shiftop) => Inst::AluRRRShift {
            alu_op: op,
            rd,
            rn,
            rm,
            shiftop,
        },
        ResultRSEImm12::RegExtend(rm, extendop) => Inst::AluRRRExtend {
            alu_op: op,
            rd,
            rn,
            rm,
            extendop,
        },
    }
}

//============================================================================
// Lowering: addressing mode support. Takes instruction directly, rather
// than an `InsnInput`, to do more introspection.

/// Lower the address of a load or store.
fn lower_address<'a, C: LowerCtx<Inst>>(
    ctx: &'a mut C,
    elem_ty: Type,
    addends: &[InsnInput],
    offset: i32,
) -> MemArg {
    // TODO: support base_reg + scale * index_reg. For this, we would need to pattern-match shl or
    // mul instructions (Load/StoreComplex don't include scale factors).

    // Handle one reg and offset that fits in immediate, if possible.
    if addends.len() == 1 {
        let reg = input_to_reg(ctx, addends[0], NarrowValueMode::ZeroExtend);
        if let Some(memarg) = MemArg::reg_maybe_offset(reg, offset as i64, elem_ty) {
            return memarg;
        }
    }

    // Handle two regs and a zero offset, if possible.
    if addends.len() == 2 && offset == 0 {
        let ra = input_to_reg(ctx, addends[0], NarrowValueMode::ZeroExtend);
        let rb = input_to_reg(ctx, addends[1], NarrowValueMode::ZeroExtend);
        return MemArg::BasePlusReg(ra, rb);
    }

    // Otherwise, generate add instructions.
    let addr = ctx.tmp(RegClass::I64, I64);

    // Get the const into a reg.
    lower_constant(ctx, addr.clone(), offset as u64);

    // Add each addend to the address.
    for addend in addends {
        let reg = input_to_reg(ctx, *addend, NarrowValueMode::ZeroExtend);
        ctx.emit(Inst::AluRRR {
            alu_op: ALUOp::Add64,
            rd: addr.clone(),
            rn: addr.to_reg(),
            rm: reg.clone(),
        });
    }

    MemArg::Base(addr.to_reg())
}

fn lower_constant<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, rd: Writable<Reg>, value: u64) {
    if let Some(imm) = MoveWideConst::maybe_from_u64(value) {
        // 16-bit immediate (shifted by 0, 16, 32 or 48 bits) in MOVZ
        ctx.emit(Inst::MovZ { rd, imm });
    } else if let Some(imm) = MoveWideConst::maybe_from_u64(!value) {
        // 16-bit immediate (shifted by 0, 16, 32 or 48 bits) in MOVN
        ctx.emit(Inst::MovN { rd, imm });
    } else if let Some(imml) = ImmLogic::maybe_from_u64(value) {
        // Weird logical-instruction immediate in ORI using zero register
        ctx.emit(Inst::AluRRImmLogic {
            alu_op: ALUOp::Orr64,
            rd,
            rn: zero_reg(),
            imml,
        });
    } else {
        // 64-bit constant in constant pool
        let const_data = u64_constant(value);
        ctx.emit(Inst::ULoad64 {
            rd,
            mem: MemArg::label(MemLabel::ConstantData(const_data)),
        });
    }
}

fn lower_condcode(cc: IntCC) -> Cond {
    match cc {
        IntCC::Equal => Cond::Eq,
        IntCC::NotEqual => Cond::Ne,
        IntCC::SignedGreaterThanOrEqual => Cond::Ge,
        IntCC::SignedGreaterThan => Cond::Gt,
        IntCC::SignedLessThanOrEqual => Cond::Le,
        IntCC::SignedLessThan => Cond::Lt,
        IntCC::UnsignedGreaterThanOrEqual => Cond::Hs,
        IntCC::UnsignedGreaterThan => Cond::Hi,
        IntCC::UnsignedLessThanOrEqual => Cond::Ls,
        IntCC::UnsignedLessThan => Cond::Lo,
        IntCC::Overflow => Cond::Vs,
        IntCC::NotOverflow => Cond::Vc,
    }
}

//=============================================================================
// Top-level instruction lowering entry point, for one instruction.

/// Actually codegen an instruction's results into registers.
fn lower_insn_to_regs<'a, C: LowerCtx<Inst>>(ctx: &'a mut C, insn: IRInst) {
    let op = ctx.data(insn).opcode();
    let inputs: SmallVec<[InsnInput; 4]> = (0..ctx.num_inputs(insn))
        .map(|i| InsnInput { insn, input: i })
        .collect();
    let outputs: SmallVec<[InsnOutput; 2]> = (0..ctx.num_outputs(insn))
        .map(|i| InsnOutput { insn, output: i })
        .collect();
    let ty = if outputs.len() > 0 {
        Some(ctx.output_ty(insn, 0))
    } else {
        None
    };

    match op {
        Opcode::Iconst | Opcode::Bconst | Opcode::F32const | Opcode::F64const | Opcode::Null => {
            let value = output_to_const(ctx, outputs[0]).unwrap();
            let rd = output_to_reg(ctx, outputs[0]);
            lower_constant(ctx, rd, value);
        }
        Opcode::Iadd => {
            let rd = output_to_reg(ctx, outputs[0]);
            let rn = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = input_to_rse_imm12(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Add32, ALUOp::Add64);
            ctx.emit(alu_inst_imm12(alu_op, rd, rn, rm));
        }
        Opcode::Isub => {
            let rd = output_to_reg(ctx, outputs[0]);
            let rn = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = input_to_rse_imm12(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
            ctx.emit(alu_inst_imm12(alu_op, rd, rn, rm));
        }

        Opcode::UaddSat | Opcode::SaddSat => {
            // TODO: open-code a sequence: adds, then branch-on-no-overflow
            // over a load of the saturated value.
            // or .. can this be done on the SIMD side?
        }

        Opcode::UsubSat | Opcode::SsubSat => {
            // TODO
        }

        Opcode::Ineg => {
            let rd = output_to_reg(ctx, outputs[0]);
            let rn = zero_reg();
            let rm = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
            ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
        }

        Opcode::Imul => {
            let rd = output_to_reg(ctx, outputs[0]);
            let rn = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = input_to_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::MAdd32, ALUOp::MAdd64);
            ctx.emit(Inst::AluRRRR {
                alu_op,
                rd,
                rn,
                rm,
                ra: zero_reg(),
            });
        }

        Opcode::Umulhi | Opcode::Smulhi => {
            let _ty = ty.unwrap();
            // TODO
        }

        Opcode::Udiv | Opcode::Sdiv | Opcode::Urem | Opcode::Srem => {
            // TODO
        }

        Opcode::Uextend | Opcode::Sextend => {
            let output_ty = ty.unwrap();
            let input_ty = ctx.input_ty(insn, 0);
            let from_bits = ty_bits(input_ty) as u8;
            let to_bits = ty_bits(output_ty) as u8;
            let to_bits = std::cmp::max(32, to_bits);
            assert!(from_bits <= to_bits);
            if from_bits < to_bits {
                let signed = op == Opcode::Sextend;
                // If we reach this point, we weren't able to incorporate the extend as
                // a register-mode on another instruction, so we have a 'None'
                // narrow-value/extend mode here, and we emit the explicit instruction.
                let rn = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
                let rd = output_to_reg(ctx, outputs[0]);
                ctx.emit(Inst::Extend {
                    rd,
                    rn,
                    signed,
                    from_bits,
                    to_bits,
                });
            }
        }

        Opcode::Band
        | Opcode::Bor
        | Opcode::Bxor
        | Opcode::Bnot
        | Opcode::BandNot
        | Opcode::BorNot
        | Opcode::BxorNot => {
            // TODO
        }

        Opcode::Rotl | Opcode::Rotr => {
            // TODO
        }

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
            // TODO
        }

        Opcode::Bitrev => {
            // TODO
        }

        Opcode::Clz | Opcode::Cls | Opcode::Ctz | Opcode::Popcnt => {
            // TODO
        }

        Opcode::Load
        | Opcode::Uload8
        | Opcode::Sload8
        | Opcode::Uload16
        | Opcode::Sload16
        | Opcode::Uload32
        | Opcode::Sload32
        | Opcode::LoadComplex
        | Opcode::Uload8Complex
        | Opcode::Sload8Complex
        | Opcode::Uload16Complex
        | Opcode::Sload16Complex
        | Opcode::Uload32Complex
        | Opcode::Sload32Complex => {
            let off = ldst_offset(ctx.data(insn)).unwrap();
            let elem_ty = match op {
                Opcode::Sload8 | Opcode::Uload8 | Opcode::Sload8Complex | Opcode::Uload8Complex => {
                    I8
                }
                Opcode::Sload16
                | Opcode::Uload16
                | Opcode::Sload16Complex
                | Opcode::Uload16Complex => I16,
                Opcode::Sload32
                | Opcode::Uload32
                | Opcode::Sload32Complex
                | Opcode::Uload32Complex => I32,
                Opcode::Load | Opcode::LoadComplex => I64,
                _ => unreachable!(),
            };

            let mem = lower_address(ctx, elem_ty, &inputs[..], off);
            let rd = output_to_reg(ctx, outputs[0]);

            ctx.emit(match op {
                Opcode::Uload8 | Opcode::Uload8Complex => Inst::ULoad8 { rd, mem },
                Opcode::Sload8 | Opcode::Sload8Complex => Inst::SLoad8 { rd, mem },
                Opcode::Uload16 | Opcode::Uload16Complex => Inst::ULoad16 { rd, mem },
                Opcode::Sload16 | Opcode::Sload16Complex => Inst::SLoad16 { rd, mem },
                Opcode::Uload32 | Opcode::Uload32Complex => Inst::ULoad32 { rd, mem },
                Opcode::Sload32 | Opcode::Sload32Complex => Inst::SLoad32 { rd, mem },
                Opcode::Load | Opcode::LoadComplex => Inst::ULoad64 { rd, mem },
                _ => unreachable!(),
            });
        }

        Opcode::Store
        | Opcode::Istore8
        | Opcode::Istore16
        | Opcode::Istore32
        | Opcode::StoreComplex
        | Opcode::Istore8Complex
        | Opcode::Istore16Complex
        | Opcode::Istore32Complex => {
            let off = ldst_offset(ctx.data(insn)).unwrap();
            let elem_ty = match op {
                Opcode::Istore8 | Opcode::Istore8Complex => I8,
                Opcode::Istore16 | Opcode::Istore16Complex => I16,
                Opcode::Istore32 | Opcode::Istore32Complex => I32,
                Opcode::Store | Opcode::StoreComplex => I64,
                _ => unreachable!(),
            };

            let mem = lower_address(ctx, elem_ty, &inputs[1..], off);
            let rd = input_to_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(match op {
                Opcode::Istore8 | Opcode::Istore8Complex => Inst::Store8 { rd, mem },
                Opcode::Istore16 | Opcode::Istore16Complex => Inst::Store16 { rd, mem },
                Opcode::Istore32 | Opcode::Istore32Complex => Inst::Store32 { rd, mem },
                Opcode::Store | Opcode::StoreComplex => Inst::Store64 { rd, mem },
                _ => unreachable!(),
            });
        }

        Opcode::StackLoad => {
            // TODO
        }

        Opcode::StackStore => {
            // TODO
        }

        Opcode::StackAddr => {
            // TODO
        }

        Opcode::GlobalValue => {
            // TODO
        }

        Opcode::SymbolValue => {
            // TODO
        }

        Opcode::HeapAddr => {
            // TODO
        }

        Opcode::TableAddr => {
            // TODO
        }

        Opcode::Nop => {
            // Nothing.
        }

        Opcode::Select | Opcode::Selectif => {
            // TODO.
        }

        Opcode::Bitselect => {
            // TODO.
        }

        Opcode::IsNull | Opcode::IsInvalid | Opcode::Trueif | Opcode::Trueff => {
            // TODO.
        }

        Opcode::Copy => {
            // TODO
        }

        Opcode::Breduce | Opcode::Bextend | Opcode::Bint | Opcode::Bmask => {
            // TODO
        }

        Opcode::Ireduce | Opcode::Isplit | Opcode::Iconcat => {
            // TODO
        }

        Opcode::FallthroughReturn => {
            // What is this? The definition says it's a "special
            // instruction" meant to allow falling through into an
            // epilogue that will then return; that just sounds like a
            // normal fallthrough. TODO: Do we need to handle this
            // differently?
            unimplemented!();
        }

        Opcode::Return => {
            for (i, input) in inputs.iter().enumerate() {
                // N.B.: according to the AArch64 ABI, the top bits of a register
                // (above the bits for the value's type) are undefined, so we
                // need not extend the return values.
                let reg = input_to_reg(ctx, *input, NarrowValueMode::None);
                let retval_reg = ctx.retval(i);
                ctx.emit(Inst::gen_move(retval_reg, reg));
            }
            // N.B.: the Ret itself is generated by the ABI.
        }

        Opcode::Icmp | Opcode::IcmpImm | Opcode::Ifcmp | Opcode::IfcmpImm => {
            // TODO
        }

        Opcode::JumpTableEntry => {
            // TODO
        }

        Opcode::JumpTableBase => {
            // TODO
        }

        Opcode::Debugtrap => {}

        Opcode::Trap => {}

        Opcode::Trapz | Opcode::Trapnz | Opcode::Trapif | Opcode::Trapff => {}

        Opcode::ResumableTrap => {}

        Opcode::Safepoint => {}

        Opcode::FuncAddr => {
            // TODO
        }

        Opcode::Call | Opcode::CallIndirect => {
            let (abi, inputs) = match op {
                Opcode::Call => {
                    let extname = ctx.call_target(insn).unwrap();
                    let sig = ctx.call_sig(insn).unwrap();
                    assert!(inputs.len() == sig.params.len());
                    assert!(outputs.len() == sig.returns.len());
                    (ARM64ABICall::from_func(sig, extname), &inputs[..])
                }
                Opcode::CallIndirect => {
                    let ptr = input_to_reg(ctx, inputs[0], NarrowValueMode::ZeroExtend);
                    let sig = ctx.call_sig(insn).unwrap();
                    assert!(inputs.len() - 1 == sig.params.len());
                    assert!(outputs.len() == sig.returns.len());
                    (ARM64ABICall::from_ptr(sig, ptr), &inputs[1..])
                }
                _ => unreachable!(),
            };
            for (i, input) in inputs.iter().enumerate() {
                let arg_reg = input_to_reg(ctx, *input, NarrowValueMode::ZeroExtend);
                ctx.emit(abi.gen_copy_reg_to_arg(i, arg_reg));
            }
            ctx.emit(abi.gen_call());
            for (i, output) in outputs.iter().enumerate() {
                let retval_reg = output_to_reg(ctx, *output);
                ctx.emit(abi.gen_copy_retval_to_reg(i, retval_reg));
            }
        }

        Opcode::GetPinnedReg
        | Opcode::SetPinnedReg
        | Opcode::Spill
        | Opcode::Fill
        | Opcode::FillNop
        | Opcode::Regmove
        | Opcode::CopySpecial
        | Opcode::CopyToSsa
        | Opcode::CopyNop
        | Opcode::AdjustSpDown
        | Opcode::AdjustSpUpImm
        | Opcode::AdjustSpDownImm
        | Opcode::IfcmpSp
        | Opcode::Regspill
        | Opcode::Regfill => {
            panic!("Unused opcode should not be encountered.");
        }

        // TODO: cmp
        // TODO: more alu ops
        Opcode::Jump
        | Opcode::Fallthrough
        | Opcode::Brz
        | Opcode::Brnz
        | Opcode::BrIcmp
        | Opcode::Brif
        | Opcode::Brff
        | Opcode::IndirectJumpTableBr
        | Opcode::BrTable => {
            panic!("Branch opcode reached non-branch lowering logic!");
        }

        Opcode::Vconst
        | Opcode::Shuffle
        | Opcode::Vsplit
        | Opcode::Vconcat
        | Opcode::Vselect
        | Opcode::VanyTrue
        | Opcode::VallTrue
        | Opcode::Splat
        | Opcode::Insertlane
        | Opcode::Extractlane
        | Opcode::Bitcast
        | Opcode::RawBitcast
        | Opcode::ScalarToVector => {
            // TODO
            panic!("Vector ops not implemented.");
        }

        Opcode::Fcmp
        | Opcode::Ffcmp
        | Opcode::Fadd
        | Opcode::Fsub
        | Opcode::Fmul
        | Opcode::Fdiv
        | Opcode::Sqrt
        | Opcode::Fma
        | Opcode::Fneg
        | Opcode::Fabs
        | Opcode::Fcopysign
        | Opcode::Fmin
        | Opcode::Fmax
        | Opcode::Ceil
        | Opcode::Floor
        | Opcode::Trunc
        | Opcode::Nearest
        | Opcode::Fpromote
        | Opcode::Fdemote
        | Opcode::FcvtToUint
        | Opcode::FcvtToUintSat
        | Opcode::FcvtToSint
        | Opcode::FcvtToSintSat
        | Opcode::FcvtFromUint
        | Opcode::FcvtFromSint => {
            panic!("Floating point ops not implemented.");
        }

        Opcode::IaddImm
        | Opcode::ImulImm
        | Opcode::UdivImm
        | Opcode::SdivImm
        | Opcode::UremImm
        | Opcode::SremImm
        | Opcode::IrsubImm
        | Opcode::IaddCin
        | Opcode::IaddIfcin
        | Opcode::IaddCout
        | Opcode::IaddIfcout
        | Opcode::IaddCarry
        | Opcode::IaddIfcarry
        | Opcode::IsubBin
        | Opcode::IsubIfbin
        | Opcode::IsubBout
        | Opcode::IsubIfbout
        | Opcode::IsubBorrow
        | Opcode::IsubIfborrow
        | Opcode::BandImm
        | Opcode::BorImm
        | Opcode::BxorImm
        | Opcode::RotlImm
        | Opcode::RotrImm
        | Opcode::IshlImm
        | Opcode::UshrImm
        | Opcode::SshrImm => {
            panic!("ALU+imm and ALU+carry ops should not appear here!");
        }

        Opcode::X86Udivmodx
        | Opcode::X86Sdivmodx
        | Opcode::X86Umulx
        | Opcode::X86Smulx
        | Opcode::X86Cvtt2si
        | Opcode::X86Fmin
        | Opcode::X86Fmax
        | Opcode::X86Push
        | Opcode::X86Pop
        | Opcode::X86Bsr
        | Opcode::X86Bsf
        | Opcode::X86Pshufd
        | Opcode::X86Pshufb
        | Opcode::X86Pextr
        | Opcode::X86Pinsr
        | Opcode::X86Insertps
        | Opcode::X86Movsd
        | Opcode::X86Movlhps
        | Opcode::X86Psll
        | Opcode::X86Psrl
        | Opcode::X86Psra
        | Opcode::X86Ptest
        | Opcode::X86Pmaxs
        | Opcode::X86Pmaxu
        | Opcode::X86Pmins
        | Opcode::X86Pminu => {
            panic!("x86-specific opcode in supposedly arch-neutral IR!");
        }
    }
}

//=============================================================================
// Helpers for instruction lowering.

fn ty_bits(ty: Type) -> usize {
    match ty {
        B1 => 1,
        B8 | I8 => 8,
        B16 | I16 => 16,
        B32 | I32 | F32 => 32,
        B64 | I64 | F64 => 64,
        B128 | I128 => 128,
        _ => panic!("ty_bits() on unknown type: {:?}", ty),
    }
}

fn choose_32_64(ty: Type, op32: ALUOp, op64: ALUOp) -> ALUOp {
    let bits = ty_bits(ty);
    if bits <= 32 {
        op32
    } else if bits == 64 {
        op64
    } else {
        panic!("choose_32_64 on > 64 bits!")
    }
}

fn branch_target(data: &InstructionData) -> Option<Block> {
    match data {
        &InstructionData::BranchIcmp { destination, .. }
        | &InstructionData::Branch { destination, .. }
        | &InstructionData::BranchInt { destination, .. }
        | &InstructionData::Jump { destination, .. }
        | &InstructionData::BranchTable { destination, .. }
        | &InstructionData::BranchFloat { destination, .. } => Some(destination),
        _ => {
            assert!(!data.opcode().is_branch());
            None
        }
    }
}

fn ldst_offset(data: &InstructionData) -> Option<i32> {
    match data {
        &InstructionData::Load { offset, .. }
        | &InstructionData::StackLoad { offset, .. }
        | &InstructionData::LoadComplex { offset, .. }
        | &InstructionData::Store { offset, .. }
        | &InstructionData::StackStore { offset, .. }
        | &InstructionData::StoreComplex { offset, .. } => Some(offset.into()),
        _ => None,
    }
}

fn inst_condcode(data: &InstructionData) -> Option<IntCC> {
    match data {
        &InstructionData::IntCond { cond, .. }
        | &InstructionData::BranchIcmp { cond, .. }
        | &InstructionData::IntCompare { cond, .. }
        | &InstructionData::IntCondTrap { cond, .. }
        | &InstructionData::BranchInt { cond, .. }
        | &InstructionData::IntSelect { cond, .. }
        | &InstructionData::IntCompareImm { cond, .. } => Some(cond),
        _ => None,
    }
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for Arm64Backend {
    type MInst = Inst;

    fn lower<C: LowerCtx<Inst>>(&self, ctx: &mut C, ir_inst: IRInst) {
        lower_insn_to_regs(ctx, ir_inst);
    }

    fn lower_branch_group<C: LowerCtx<Inst>>(
        &self,
        ctx: &mut C,
        branches: &[IRInst],
        targets: &[BlockIndex],
        fallthrough: Option<BlockIndex>,
    ) {
        // A block should end with at most two branches. The first may be a
        // conditional branch; a conditional branch can be followed only by an
        // unconditional branch or fallthrough. Otherwise, if only one branch,
        // it may be an unconditional branch, a fallthrough, a return, or a
        // trap. These conditions are verified by `is_ebb_basic()` during the
        // verifier pass.
        assert!(branches.len() <= 2);

        if branches.len() == 2 {
            // Must be a conditional branch followed by an unconditional branch.
            let op0 = ctx.data(branches[0]).opcode();
            let op1 = ctx.data(branches[1]).opcode();

            //println!(
            //    "lowering two-branch group: opcodes are {:?} and {:?}",
            //    op0, op1
            //);

            assert!(op1 == Opcode::Jump || op1 == Opcode::Fallthrough);
            let taken = BranchTarget::Block(targets[0]);
            let not_taken = match op1 {
                Opcode::Jump => BranchTarget::Block(targets[1]),
                Opcode::Fallthrough => BranchTarget::Block(fallthrough.unwrap()),
                _ => unreachable!(), // assert above.
            };
            match op0 {
                Opcode::Brz | Opcode::Brnz => {
                    let rt = input_to_reg(
                        ctx,
                        InsnInput {
                            insn: branches[0],
                            input: 0,
                        },
                        NarrowValueMode::ZeroExtend,
                    );
                    let kind = match op0 {
                        Opcode::Brz => CondBrKind::Zero(rt),
                        Opcode::Brnz => CondBrKind::NotZero(rt),
                        _ => unreachable!(),
                    };
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                }
                Opcode::BrIcmp => {
                    let cond = lower_condcode(inst_condcode(ctx.data(branches[0])).unwrap());
                    let rn = input_to_reg(
                        ctx,
                        InsnInput {
                            insn: branches[0],
                            input: 0,
                        },
                        // TODO: verify that this is correct in all cases.
                        NarrowValueMode::SignExtend,
                    );
                    let rm = input_to_reg(
                        ctx,
                        InsnInput {
                            insn: branches[0],
                            input: 1,
                        },
                        NarrowValueMode::SignExtend,
                    );
                    let ty = ctx.input_ty(branches[0], 0);
                    let alu_op = choose_32_64(ty, ALUOp::SubS32, ALUOp::SubS64);
                    let rd = writable_zero_reg();
                    ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                }

                // TODO: Brif/icmp, Brff/icmp, jump tables
                _ => unimplemented!(),
            }
        } else {
            assert!(branches.len() == 1);

            // Must be an unconditional branch or trap.
            let op = ctx.data(branches[0]).opcode();
            match op {
                Opcode::Jump => {
                    ctx.emit(Inst::Jump {
                        dest: BranchTarget::Block(targets[0]),
                    });
                }
                Opcode::Fallthrough => {
                    ctx.emit(Inst::Jump {
                        dest: BranchTarget::Block(targets[0]),
                    });
                }

                Opcode::Trap => unimplemented!(),

                _ => panic!("Unknown branch type!"),
            }
        }
    }
}
