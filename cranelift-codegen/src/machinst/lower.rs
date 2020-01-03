//! This module implements lowering (instruction selection) from Cranelift IR to machine
//! instructions with virtual registers, with lookup tables as built by the backend. This is
//! *almost* the final machine code, except for register allocation.

use crate::entity::SecondaryMap;
use crate::ir::{Ebb, Function, Inst, InstructionData, Type, Value, ValueDef};
use crate::isa::registers::{RegClass, RegUnit};
use crate::machinst::MachReg;
use crate::num_uses::NumUses;

use alloc::vec::Vec;
use smallvec::SmallVec;

/// A context that machine-specific lowering code can use to emit lowered instructions.
pub trait LowerCtx<I> {
    /// Get the instdata for a given IR instruction.
    fn data(&self, ir_inst: Inst) -> &InstructionData;
    /// Get the controlling type for a polymorphic IR instruction.
    fn ty(&self, ir_inst: Inst) -> Type;
    /// Emit a machine instruction.
    fn emit(&mut self, mach_inst: I);
    /// Reduce the use-count of an IR instruction. Use this when, e.g., isel incorporates the
    /// computation of an input instruction directly.
    fn dec_use(&mut self, ir_inst: Inst);
    /// Get the producing instruction, if any, and output number, for the `idx`th input to the
    /// given IR instruction
    fn input_inst(&self, ir_inst: Inst, idx: usize) -> Option<(Inst, usize)>;
    /// Get the `idx`th input to the given IR instruction as a virtual register.
    fn input(&self, ir_inst: Inst, idx: usize) -> MachReg;
    /// Get the `idx`th output of the given IR instruction as a virtual register.
    fn output(&self, ir_inst: Inst, idx: usize) -> MachReg;
    /// Get the number of inputs to the given IR instruction.
    fn num_inputs(&self, ir_inst: Inst) -> usize;
    /// Get the number of outputs to the given IR instruction.
    fn num_outputs(&self, ir_inst: Inst) -> usize;
    /// Get the type for an instruction's input.
    fn input_ty(&self, ir_inst: Inst, idx: usize) -> Type;
    /// Get the type for an instruction's output.
    fn output_ty(&self, ir_inst: Inst, idx: usize) -> Type;
    /// Get a new temp.
    fn tmp(&mut self, rc: RegClass) -> MachReg;
    /// Get the register for an EBB param.
    fn ebb_param(&self, ebb: Ebb, idx: usize) -> MachReg;
}

/// A backend's lowering logic, to be driven by the machine-independent portion of instruction
/// lowering.
pub trait LowerBackend {
    /// The machine instruction type.
    type MInst;

    /// Lower a single instruction. Instructions are lowered in reverse order.
    fn lower(&mut self, ctx: &mut dyn LowerCtx<Self::MInst>, inst: Inst);
}

/// Machine-independent lowering driver / machine-instruction container. Maintains a correspondence
/// from original Inst to MachInsts.
pub struct Lower<'a, I> {
    // The function to lower.
    f: &'a Function,

    // Lowered machine instructions. In arbitrary order; map from original IR program order using
    // `inst_indices` below.
    insts: Vec<I>,

    // Number of active uses (minus `dec_use()` calls by backend) of each instruction.
    num_uses: SecondaryMap<Inst, u32>,

    // Range of indices in `insts` corresponding to a given Cranelift instruction:
    inst_indices: SecondaryMap<Inst, (u32, u32)>,

    // Mapping from `Value` (SSA value in IR) to virtual register.
    value_regs: SecondaryMap<Value, MachReg>,

    // Next virtual register number to allocate.
    next_vreg: usize,

    // Current IR instruction which we are lowering.
    cur_inst: Option<Inst>,
}

fn alloc_vreg(value_regs: &mut SecondaryMap<Value, MachReg>, value: Value, next_vreg: &mut usize) {
    match value_regs[value] {
        MachReg::Undefined => {
            let v = *next_vreg;
            *next_vreg += 1;
            value_regs[value] = MachReg::Virtual(v);
        }
        _ => {}
    }
}

impl<'a, I> Lower<'a, I> {
    /// Prepare a new lowering context for the given IR function.
    pub fn new(f: &'a Function) -> Lower<'a, I> {
        let num_uses = NumUses::compute(f).take_uses();

        let mut next_vreg = 0;
        let mut value_regs = SecondaryMap::with_default(MachReg::Undefined);
        for ebb in f.layout.ebbs() {
            for param in f.dfg.ebb_params(ebb) {
                alloc_vreg(&mut value_regs, *param, &mut next_vreg);
            }
            for inst in f.layout.ebb_insts(ebb) {
                for arg in f.dfg.inst_args(inst) {
                    alloc_vreg(&mut value_regs, *arg, &mut next_vreg);
                }
                for result in f.dfg.inst_results(inst) {
                    alloc_vreg(&mut value_regs, *result, &mut next_vreg);
                }
            }
        }

        Lower {
            f,
            insts: vec![],
            num_uses,
            inst_indices: SecondaryMap::with_default((0, 0)),
            value_regs,
            next_vreg,
            cur_inst: None,
        }
    }

    /// Lower the function.
    pub fn lower(&mut self, backend: &mut dyn LowerBackend<MInst = I>) {
        // Work backward (postorder for EBBs, reverse through each EBB), skipping insns with
        // zero uses.
        let ebbs: SmallVec<[Ebb; 16]> = self.f.layout.ebbs().collect();
        for ebb in ebbs.into_iter().rev() {
            for inst in self.f.layout.ebb_insts(ebb).rev() {
                if self.num_uses[inst] > 0 {
                    self.start_inst(inst);
                    backend.lower(self, inst);
                    self.end_inst();
                }
            }
        }
    }

    fn start_inst(&mut self, inst: Inst) {
        self.cur_inst = Some(inst);
        let l = self.insts.len() as u32;
        self.inst_indices[inst] = (l, l);
    }

    fn end_inst(&mut self) {
        self.cur_inst = None;
    }
}

impl<'a, I> LowerCtx<I> for Lower<'a, I> {
    /// Get the instdata for a given IR instruction.
    fn data(&self, ir_inst: Inst) -> &InstructionData {
        &self.f.dfg[ir_inst]
    }

    /// Get the controlling type for a polymorphic IR instruction.
    fn ty(&self, ir_inst: Inst) -> Type {
        self.f.dfg.ctrl_typevar(ir_inst)
    }

    /// Emit a machine instruction.
    fn emit(&mut self, mach_inst: I) {
        let cur_inst = self.cur_inst.clone().unwrap();
        self.insts.push(mach_inst);
        // Bump the end of the range.
        self.inst_indices[cur_inst].1 = self.insts.len() as u32;
    }

    /// Reduce the use-count of an IR instruction. Use this when, e.g., isel incorporates the
    /// computation of an input instruction directly.
    fn dec_use(&mut self, ir_inst: Inst) {
        assert!(self.num_uses[ir_inst] > 0);
        self.num_uses[ir_inst] -= 1;
    }

    /// Get the producing instruction, if any, and output number, for the `idx`th input to the
    /// given IR instruction.
    fn input_inst(&self, ir_inst: Inst, idx: usize) -> Option<(Inst, usize)> {
        let val = self.f.dfg.inst_args(ir_inst)[idx];
        match self.f.dfg.value_def(val) {
            ValueDef::Result(src_inst, result_idx) => Some((src_inst, result_idx)),
            _ => None,
        }
    }

    /// Get the `idx`th input to the given IR instruction as a virtual register.
    fn input(&self, ir_inst: Inst, idx: usize) -> MachReg {
        let val = self.f.dfg.inst_args(ir_inst)[idx];
        self.value_regs[val]
    }

    /// Get the `idx`th output of the given IR instruction as a virtual register.
    fn output(&self, ir_inst: Inst, idx: usize) -> MachReg {
        let val = self.f.dfg.inst_results(ir_inst)[idx];
        self.value_regs[val]
    }

    /// Get a new temp.
    fn tmp(&mut self, rc: RegClass) -> MachReg {
        let v = self.next_vreg;
        self.next_vreg += 1;
        MachReg::Virtual(v)
    }

    /// Get the number of inputs for the given IR instruction.
    fn num_inputs(&self, ir_inst: Inst) -> usize {
        self.f.dfg.inst_args(ir_inst).len()
    }

    /// Get the number of outputs for the given IR instruction.
    fn num_outputs(&self, ir_inst: Inst) -> usize {
        self.f.dfg.inst_results(ir_inst).len()
    }

    /// Get the type for an instruction's input.
    fn input_ty(&self, ir_inst: Inst, idx: usize) -> Type {
        self.f.dfg.value_type(self.f.dfg.inst_args(ir_inst)[idx])
    }

    /// Get the type for an instruction's output.
    fn output_ty(&self, ir_inst: Inst, idx: usize) -> Type {
        self.f.dfg.value_type(self.f.dfg.inst_results(ir_inst)[idx])
    }

    /// Get the register for an EBB param.
    fn ebb_param(&self, ebb: Ebb, idx: usize) -> MachReg {
        let val = self.f.dfg.ebb_params(ebb)[idx];
        self.value_regs[val]
    }
}

// TODO: impl RegAllocView for Lower.
// - iterate over insns (CFG? domtree?)