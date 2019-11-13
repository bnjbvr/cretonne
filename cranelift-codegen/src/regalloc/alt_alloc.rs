//! If the earth really were flat, cats would have pushed everything off the edge by now.
//! Hence I conclude the earth is not flat.

use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;

use log::info;

use crate::cursor::{Cursor, EncCursor};
use crate::dominator_tree::DominatorTree;
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{
    AbiParam, ArgumentLoc, Ebb, Function, Inst, InstBuilder, InstructionData, Opcode,
    StackSlotKind, Type, Value, ValueLoc,
};
use crate::isa::{ConstraintKind, EncInfo, TargetIsa};
use crate::topo_order::TopoOrder;

use crate::isa::registers::{RegClass, RegUnit};
use crate::regalloc::branch_splitting;
use crate::regalloc::register_set::RegisterSet;

use crate::entity::{EntityList, ListPool, PrimaryMap, SecondaryMap};
use crate::regalloc::virtregs::VirtReg;

// ========================================================================================== //
//                                                                                            //
// Lars' Minimal Allocator                                                                    //
//                                                                                            //
// ========================================================================================== //

// Minimal register allocator.
//
// The `minimal` register allocator assigns every Value in the incoming program to a unique stack
// slot, then moves values into registers only as required by each instruction, and finally moves
// any values defined by the instruction out of registers directly after the instruction.
//
// The values that are in registers are new Value slots, and the instructions are updated to take
// these new Values as arguments and produce them as results.  Value movement is through fill and
// spill instructions.
//
// The allocator must handle the function ABI and two-address operations (tied registers) and must
// obey all instruction constraints (eg fixed registers and register classes), but is otherwise the
// simplest register allocator imaginable for our given IR structure.
//
// The allocator requires that conditional branch exits that pass parameters have been split, ie,
// that the branch parameters have been removed by branching without parameters to an intermediary
// block that performs a jump with the parameters.

// TODO: Can the flags hack be generalized?  The coloring regalloc does not need this test.
//       The isa has a uses_cpu_flags() thing that might be a useful guard?  Only postopt.rs
//         uses this to guard flags-specific optimizations.
//       There's the clobbers_flags on the RecipeConstraints, probably not what we want.
//       verifier/flags.rs and postopt.rs are interesting but not enlightening, exactly.
//       Some tests have interesting rc_by_name functionality that selects specific
//         register classes, probably not portable but it's possible the GC allocator
//         selects non-flag classes specifically for something.
//       I guess flags are sort of avoided if one looks at the results of the instruction
//         "just so", eg in terms of the live value tracker
//       => Could it be that the ValueLoc is already assigned in all cases for ValueLoc::Reg?
//          But then what is the Reg?
//
// TODO: Feels like there are a few too many special-purpose tests and cases?
//
// TODO: The register set abstraction is probably quite slow, since it creates an iterator
//       for pretty much every allocation; there are better ways.

// =============================================================================
// Minimal Allocator: state

// It apparently has no state.
//struct MinimalState {}

// =============================================================================
// Minimal Allocator: registers

struct Regs {
    registers: RegisterSet,
}

impl Regs {
    fn new(registers: RegisterSet) -> Self {
        Self { registers }
    }
    fn take_specific(&mut self, rc: RegClass, r: RegUnit) {
        self.registers.take(rc, r);
    }
    fn take(&mut self, rc: RegClass) -> RegUnit {
        let mut i = self.registers.iter(rc);
        let reg = i.next().expect("no available register");
        self.registers.take(rc, reg);
        reg
    }
    fn free(&mut self, rc: RegClass, r: RegUnit) {
        self.registers.free(rc, r);
    }
}

// =============================================================================
// Minimal Allocator: processing of instruction fragments

impl<'a> Context<'a> {
    /// Create a new Value allocated to a register and load `val` into it.
    fn fill_temp_register(&mut self, val: Value, regs: &mut Regs) -> (Value, RegClass, RegUnit) {
        let temp = self.cur.ins().fill(val);
        let fill = self.cur.built_inst();
        let enc = self.cur.func.encodings[fill];
        let constraints = self.encinfo.operand_constraints(enc).unwrap();
        let rc = constraints.ins[0].regclass;
        let reg = regs.take(rc);
        self.cur.func.locations[temp] = ValueLoc::Reg(reg);
        (temp, rc, reg)
    }

    fn spill_register(
        &mut self,
        reg: RegUnit,
        regname: Value,
        stackname: Value,
        value_type: Type,
    ) -> Inst {
        self.cur.func.locations[regname] = ValueLoc::Reg(reg);
        self.cur.ins().with_result(stackname).spill(regname);
        let spill = self.cur.built_inst();
        let ss = self.cur.func.stack_slots.make_spill_slot(value_type);
        self.cur.func.locations[stackname] = ValueLoc::Stack(ss);
        spill
    }

    fn spill_result_from_register(&mut self, result: Value, reg: RegUnit) -> Inst {
        let value_type = self.cur.func.dfg.value_type(result);
        let new_result = self.cur.func.dfg.replace_result(result, value_type);
        self.spill_register(reg, new_result, result, value_type)
    }

    // This will assume that tied registers are already allocated.  It leaves the point at the last
    // instruction inserted after `inst`, if any.
    fn spill_register_results(
        &mut self,
        inst: Inst,
        regs: &mut Regs,
        reg_args: Vec<(usize, Value, RegClass, RegUnit, bool)>,
    ) {
        let constraints = self
            .encinfo
            .operand_constraints(self.cur.func.encodings[inst]);

        // Reserve any fixed output registers that are not also tied.
        if let Some(constraints) = constraints {
            if constraints.fixed_outs {
                for constraint in constraints.outs {
                    match constraint.kind {
                        ConstraintKind::FixedReg(r) => regs.take_specific(constraint.regclass, r),
                        _ => {}
                    }
                }
            }
        }

        // Assign the output registers.
        let mut reg_results = vec![];
        for (k, result) in self.cur.func.dfg.inst_results(inst).iter().enumerate() {
            let constraint = &constraints.unwrap().outs[k];
            debug_assert!(constraint.kind != ConstraintKind::Stack);
            let (rc, reg) = match constraint.kind {
                ConstraintKind::FixedTied(r) => (constraint.regclass, r),
                ConstraintKind::FixedReg(r) => (constraint.regclass, r),
                ConstraintKind::Tied(input) => {
                    let hit = *reg_args
                        .iter()
                        .filter(|(input_k, ..)| *input_k == input as usize)
                        .next()
                        .unwrap();
                    debug_assert!(hit.4);
                    (hit.2, hit.3)
                }
                ConstraintKind::Reg => (constraint.regclass, regs.take(constraint.regclass)),
                ConstraintKind::Stack => unreachable!(),
            };
            reg_results.push((k, *result, rc, reg));
        }

        // Insert spills, assign locations, update the instruction, free registers.
        let mut last = inst;
        self.cur.goto_after_inst(inst);
        for (_k, result, rc, reg) in reg_results {
            let value_type = self.cur.func.dfg.value_type(result);
            if value_type.is_flags() {
                self.cur.func.locations[result] = ValueLoc::Reg(reg);
            } else {
                last = self.spill_result_from_register(result, reg);
                self.cur.goto_after_inst(last);
            }

            regs.free(rc, reg);
        }
        self.cur.goto_inst(last);
    }

    // Parallel assignment for unconditional control flow.
    //
    // If a source value uses the same stack slot as a target value (which can happen along a back
    // edge), we must take care not to write a target slot that is needed subsequently as a source.
    // In the limit, there can be a circularity, with target (x,y) and ebb arguments (y,x), say.
    //
    // (A detail: Since our implementation of COPY introduces an alias we can't disambiguate by the
    // value names; we must instead disambiguate by the actual stack slots they reference.)
    //
    // We can solve this trivially by introducing temps for all the arguments: copy into temps, then
    // copy into target slots.  Better, but still simple, is to introduce temps only for stack slots
    // that appear in both the source and target lists, without worrying further about copy order.
    // As a simple optimization we avoid a copy when the source and target slots are the same slot.
    // This will not free any tied registers it allocates.  It leaves the point at `inst`.  The
    // return value reflects the allocated registers (all of them), some of which may no longer have
    // been taken from regs.
    fn fill_register_args(
        &mut self,
        inst: Inst,
        regs: &mut Regs,
        fixed: bool,
    ) -> Vec<(usize, Value, RegClass, RegUnit, bool)> {
        let constraints = self
            .encinfo
            .operand_constraints(self.cur.func.encodings[inst]);

        // Reserve any fixed input registers.
        if let Some(constraints) = constraints {
            if constraints.fixed_ins {
                for constraint in constraints.ins {
                    match constraint.kind {
                        ConstraintKind::FixedReg(r) => regs.take_specific(constraint.regclass, r),
                        ConstraintKind::FixedTied(r) => regs.take_specific(constraint.regclass, r),
                        _ => {}
                    }
                }
            }
        }

        // Assign all input registers.
        let mut reg_args = vec![];
        for (k, arg) in if fixed {
            self.cur.func.dfg.inst_fixed_args(inst)
        } else {
            self.cur.func.dfg.inst_args(inst)
        }
        .iter()
        .enumerate()
        {
            debug_assert!(
                if let ValueLoc::Stack(_ss) = self.cur.func.locations[*arg] {
                    true
                } else {
                    self.cur.func.dfg.value_type(*arg).is_flags()
                }
            );
            let constraint = &constraints.unwrap().ins[k];
            if constraint.kind == ConstraintKind::Stack {
                continue;
            }
            let rc = constraint.regclass;
            let (reg, is_tied) = match constraint.kind {
                ConstraintKind::FixedReg(r) => (r, false),
                ConstraintKind::FixedTied(r) => (r, true),
                ConstraintKind::Tied(_) => (regs.take(rc), true),
                ConstraintKind::Reg => (regs.take(rc), false),
                ConstraintKind::Stack => unreachable!(),
            };
            reg_args.push((k, *arg, rc, reg, is_tied));
        }

        // Insert fills, assign locations, update the instruction, free registers.
        for (k, arg, rc, reg, is_tied) in &reg_args {
            let value_type = self.cur.func.dfg.value_type(*arg);
            if value_type.is_flags() {
                self.cur.func.locations[*arg] = ValueLoc::Reg(*reg);
            } else {
                let temp = self.cur.ins().fill(*arg);
                self.cur.func.locations[temp] = ValueLoc::Reg(*reg);
                if fixed {
                    self.cur.func.dfg.inst_fixed_args_mut(inst)[*k] = temp;
                } else {
                    self.cur.func.dfg.inst_args_mut(inst)[*k] = temp;
                }
            }
            if !*is_tied {
                regs.free(*rc, *reg);
            }
        }

        reg_args
    }

    fn move_ebb_arguments(&mut self, target: Ebb, inst: Inst, regs: &mut Regs) {
        let target_slots: Vec<_> = self
            .cur
            .func
            .dfg
            .ebb_params(target)
            .iter()
            .map(|i| {
                if let ValueLoc::Stack(ss) = self.cur.func.locations[*i] {
                    ss
                } else {
                    unreachable!()
                }
            })
            .collect();

        let arginfo: Vec<_> = self
            .cur
            .func
            .dfg
            .ebb_params(target)
            .iter()
            .zip(self.cur.func.dfg.inst_args(inst).iter())
            .map(|(a, b)| (*b, *a))
            .enumerate()
            .collect();

        let mut updates = vec![];
        for (k, (arg, target_arg)) in arginfo {
            let arg_loc = self.cur.func.locations[arg];
            let target_arg_loc = self.cur.func.locations[target_arg];
            if let (ValueLoc::Stack(arg_ss), ValueLoc::Stack(target_ss)) = (arg_loc, target_arg_loc)
            {
                if arg_ss == target_ss {
                    continue;
                }
                let need_stack_temp = target_slots.iter().any(|ts| arg_ss == *ts);
                if need_stack_temp {
                    let (temp, rc, reg) = self.fill_temp_register(arg, regs);
                    let the_temp = self.cur.ins().spill(temp);
                    let value_type = self.cur.func.dfg.value_type(arg);
                    let ss = self.cur.func.stack_slots.make_spill_slot(value_type);
                    self.cur.func.locations[the_temp] = ValueLoc::Stack(ss);
                    regs.free(rc, reg);
                    updates.push((k, the_temp, target_arg));
                } else {
                    updates.push((k, arg, target_arg));
                }
            } else {
                unreachable!();
            }
        }

        for (k, arg, target_arg) in updates {
            let (temp, rc, reg) = self.fill_temp_register(arg, regs);
            let dest = self.cur.ins().spill(temp);
            self.cur.func.dfg.inst_args_mut(inst)[k] = dest;
            self.cur.func.locations[dest] = self.cur.func.locations[target_arg];
            regs.free(rc, reg);
        }
    }
}

// =============================================================================
// Minimal Allocator: ABI related

impl<'a> Context<'a> {
    fn make_abi_info(&self, vals: &[Value], abi: &[AbiParam]) -> Vec<(usize, (Value, AbiParam))> {
        vals.iter()
            .zip(abi)
            .map(|(val, abi)| (*val, *abi))
            .enumerate()
            .collect()
    }

    fn take_variable_abi_registers(
        &self,
        abi_info: &[(usize, (Value, AbiParam))],
        regs: &mut Regs,
    ) {
        for (_, (val, abi)) in abi_info {
            if let ArgumentLoc::Reg(r) = abi.location {
                let ty = self.cur.func.dfg.value_type(*val);
                let rc = self.cur.isa.regclass_for_abi_type(ty).into();
                regs.take_specific(rc, r);
            }
        }
    }

    fn free_variable_abi_registers(
        &self,
        abi_info: &[(usize, (Value, AbiParam))],
        regs: &mut Regs,
    ) {
        for (_, (val, abi)) in abi_info {
            if let ArgumentLoc::Reg(r) = abi.location {
                let ty = self.cur.func.dfg.value_type(*val);
                let rc = self.cur.isa.regclass_for_abi_type(ty).into();
                regs.free(rc, r);
            }
        }
    }

    fn load_variable_abi_registers(
        &mut self,
        inst: Inst,
        abi_info: &[(usize, (Value, AbiParam))],
    ) -> bool {
        let mut to_stack = false;
        for (k, (val, abi)) in abi_info {
            match abi.location {
                ArgumentLoc::Reg(r) => {
                    let temp = self.cur.ins().fill(*val);
                    self.cur.func.locations[temp] = ValueLoc::Reg(r);
                    self.cur.func.dfg.inst_variable_args_mut(inst)[*k] = temp;
                }
                _ => {
                    to_stack = true;
                }
            }
        }
        to_stack
    }

    fn store_variable_abi_registers(
        &mut self,
        inst: Inst,
        abi_info: &[(usize, (Value, AbiParam))],
    ) -> (bool, Inst) {
        let mut from_stack = false;
        let mut last = inst;
        for (_, (result, abi)) in abi_info {
            match abi.location {
                ArgumentLoc::Reg(reg) => {
                    last = self.spill_result_from_register(*result, reg);
                    self.cur.goto_after_inst(last);
                }
                _ => {
                    from_stack = true;
                }
            }
        }
        (from_stack, last)
    }
}

// =============================================================================
// Minimal Allocator: processing of individual instructions

impl<'a> Context<'a> {
    fn visit_branch(&mut self, inst: Inst, regs: &mut Regs) {
        // Branch edges that pass parameters must have been split.
        debug_assert!({
            match self.cur.func.dfg[inst] {
                InstructionData::Branch { destination, .. }
                | InstructionData::BranchIcmp { destination, .. }
                | InstructionData::BranchInt { destination, .. }
                | InstructionData::BranchFloat { destination, .. } => {
                    self.cur.func.dfg.ebb_params(destination).len() == 0
                }
                _ => panic!("Unexpected instruction in classify_branch"),
            }
        });

        // Load the branch arguments into registers.
        self.fill_register_args(inst, regs, true);
    }

    fn visit_terminator(&mut self, inst: Inst, regs: &mut Regs, opcode: Opcode) {
        match opcode {
            Opcode::Return | Opcode::FallthroughReturn => {
                let abi_info = self.make_abi_info(
                    self.cur.func.dfg.inst_args(inst),
                    &self.cur.func.signature.returns,
                );
                let to_stack = self.load_variable_abi_registers(inst, &abi_info);
                debug_assert!(!to_stack);
            }
            Opcode::IndirectJumpTableBr => {
                self.fill_register_args(inst, regs, true);
            }
            Opcode::Jump => {
                if let InstructionData::Jump { destination, .. } = self.cur.func.dfg[inst] {
                    self.move_ebb_arguments(destination, inst, regs);
                } else {
                    panic!("Should not see a Fallthrough here");
                }
            }
            Opcode::Trap => {}
            _ => unreachable!(),
        }
    }

    fn visit_copy(&mut self, inst: Inst) {
        // As the stack slots are immutable, a copy is simply a sharing of location.  However, if we
        // just remove the instruction then its result will have no defining instruction.  So
        // rewrite as copy_nop instead.
        let arg = self.cur.func.dfg.inst_args(inst)[0];
        let dest = self.cur.func.dfg.inst_results(inst)[0];
        self.cur.func.locations[dest] = self.cur.func.locations[arg];
        self.cur.func.dfg.replace(inst).copy_nop(arg);
        let ok = self.cur.func.update_encoding(inst, self.cur.isa).is_ok();
        debug_assert!(ok, "copy_nop encoding missing for this type");
    }

    fn visit_call(&mut self, inst: Inst, regs: &mut Regs, _opcode: Opcode) {
        let sig = self.cur.func.dfg.call_signature(inst).unwrap();

        // Setup ABI register arguments
        let arg_info = self.make_abi_info(
            self.cur.func.dfg.inst_variable_args(inst),
            &self.cur.func.dfg.signatures[sig].params,
        );
        self.load_variable_abi_registers(inst, &arg_info);

        // Load fixed args, avoiding ABI registers.
        self.take_variable_abi_registers(&arg_info, regs);
        self.fill_register_args(inst, regs, true);
        self.free_variable_abi_registers(&arg_info, regs);

        // Move past the instruction
        self.cur.goto_after_inst(inst);

        // Capture ABI results
        let result_info = self.make_abi_info(
            self.cur.func.dfg.inst_results(inst),
            &self.cur.func.dfg.signatures[sig].returns,
        );
        let (from_stack, last) = self.store_variable_abi_registers(inst, &result_info);
        debug_assert!(!from_stack);

        self.cur.goto_inst(last);
    }

    fn is_spill_to_outgoing_arg(&self, inst: Inst) -> bool {
        debug_assert!(self.cur.func.dfg[inst].opcode() == Opcode::Spill);
        let result = self.cur.func.dfg.inst_results(inst)[0];
        if let ValueLoc::Stack(ss) = self.cur.func.locations[result] {
            return self.cur.func.stack_slots[ss].kind == StackSlotKind::OutgoingArg;
        }
        false
    }

    fn visit_outgoing_arg_spill(&mut self, inst: Inst, regs: &mut Regs) {
        debug_assert!(self.cur.func.dfg[inst].opcode() == Opcode::Spill);
        let arg = self.cur.func.dfg.inst_args(inst)[0];
        let (temp, rc, reg) = self.fill_temp_register(arg, regs);
        self.cur.func.dfg.inst_args_mut(inst)[0] = temp;
        regs.free(rc, reg);
    }

    fn visit_plain_inst(&mut self, inst: Inst, regs: &mut Regs) {
        let reg_args = self.fill_register_args(inst, regs, false);
        self.spill_register_results(inst, regs, reg_args);
    }

    fn visit_inst(&mut self, inst: Inst, regs: &mut Regs) {
        let opcode = self.cur.func.dfg[inst].opcode();
        match opcode {
            Opcode::Copy => {
                self.visit_copy(inst);
            }
            Opcode::BrTable
            | Opcode::Fallthrough
            | Opcode::FallthroughReturn
            | Opcode::IndirectJumpTableBr
            | Opcode::Jump
            | Opcode::Return
            | Opcode::Trap => {
                debug_assert!(opcode.is_terminator());
                self.visit_terminator(inst, regs, opcode);
            }
            Opcode::BrIcmp | Opcode::Brff | Opcode::Brif | Opcode::Brnz | Opcode::Brz => {
                debug_assert!(opcode.is_branch());
                self.visit_branch(inst, regs);
            }
            Opcode::Call | Opcode::CallIndirect => {
                debug_assert!(opcode.is_call());
                self.visit_call(inst, regs, opcode);
            }
            Opcode::Spill if self.is_spill_to_outgoing_arg(inst) => {
                self.visit_outgoing_arg_spill(inst, regs);
            }
            Opcode::Spill | Opcode::Fill => {
                // Inserted by the register allocator; ignore them.
            }
            _ => {
                // Some opcodes should not be encountered here.
                debug_assert!(
                    opcode != Opcode::Regmove
                        && opcode != Opcode::Regfill
                        && opcode != Opcode::Regspill
                        && opcode != Opcode::CopySpecial
                );
                // Make sure we covered all cases above.
                debug_assert!(!opcode.is_terminator() && !opcode.is_branch() && !opcode.is_call());
                self.visit_plain_inst(inst, regs);
            }
        }
    }
}

// =============================================================================
// Minimal Allocator: processing of blocks and the whole function

impl<'a> Context<'a> {
    fn visit_entry_block(&mut self, entry: Ebb) {
        let signature_info: Vec<_> = self
            .cur
            .func
            .dfg
            .ebb_params(entry)
            .iter()
            .zip(&self.cur.func.signature.params)
            .map(|(param, abi)| (*param, *abi))
            .collect();

        self.cur.goto_first_inst(entry);
        for (param, abi) in signature_info {
            match abi.location {
                ArgumentLoc::Reg(reg) => {
                    let new_param = self.cur.func.dfg.replace_ebb_param(param, abi.value_type);
                    self.spill_register(reg, new_param, param, abi.value_type);
                }
                ArgumentLoc::Stack(_offset) => {
                    // Incoming stack arguments have sensible pre-initialized locations.
                    debug_assert!(
                        if let ValueLoc::Stack(_ss) = self.cur.func.locations[param] {
                            true
                        } else {
                            false
                        }
                    );
                }
                ArgumentLoc::Unassigned => {
                    panic!("Should not happen");
                }
            }
        }
    }

    fn visit_other_blocks(&mut self) {
        let entry = self.cur.func.layout.entry_block().unwrap();
        self.topo.reset(self.cur.func.layout.ebbs());

        // Skip the entry block.
        let first = self.topo.next(&self.cur.func.layout, self.domtree).unwrap();
        debug_assert!(first == entry);

        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            for param in self.cur.func.dfg.ebb_params(ebb) {
                let ss = self
                    .cur
                    .func
                    .stack_slots
                    .make_spill_slot(self.cur.func.dfg.value_type(*param));
                self.cur.func.locations[*param] = ValueLoc::Stack(ss);
            }
        }
    }

    fn run_minimal_allocator(&mut self) {
        //dbg!(&self.cur.func);

        // For the entry block, spill register parameters to the stack while retaining their names.
        self.visit_entry_block(self.cur.func.layout.entry_block().unwrap());

        // For all blocks other than the entry block, assign stack slots to all block parameters so
        // that we can later process control transfer instructions.
        self.visit_other_blocks();

        // Process all instructions in domtree order so that we'll always know the location of a
        // definition when we see its use.  Fill any register args before the instruction and spill
        // any definitions after.
        let mut regs = Regs::new(self.usable_regs.clone());
        self.topo.reset(self.cur.func.layout.ebbs());
        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            self.cur.goto_top(ebb);
            while let Some(inst) = self.cur.next_inst() {
                // Resolving aliases seems necessary because the minimal alloc is not preceded by
                // the liveness allocation pass that would otherwise take care of it.
                self.cur.func.dfg.resolve_aliases_in_arguments(inst);
                if !self.cur.func.dfg[inst].opcode().is_ghost() {
                    self.visit_inst(inst, &mut regs);
                }
            }
        }

        //dbg!(&self.cur.func);
    }
}

// ========================================================================================== //
//                                                                                            //
// Printers                                                                                   //
//                                                                                            //
// ========================================================================================== //

impl<'a> Context<'a> {
    fn show(&self, limits: (Option<usize>, Option<usize>), run_number: usize, what: &str) {
        // Figure out if we actually want to print this function.
        let in_range_lo = match limits.0 {
            None => true,
            Some(not_below) => run_number >= not_below,
        };
        let in_range_hi = match limits.1 {
            None => true,
            Some(not_above) => run_number <= not_above,
        };
        if !in_range_lo || !in_range_hi {
            return;
        }

        // Ok .. print it.
        println!(
            "==== Fn {}: {} ========================================================",
            run_number, what
        );
        println!("");
        println!("{}", self.cur.func.display(self.cur.isa));
    }
}

// ========================================================================================== //
//                                                                                            //
// Alt allocator                                                                              //
//                                                                                            //
// ========================================================================================== //

// =============================================================================
// Alt allocator: running state

// The alt allocator's state
pub struct AAState {
    vregs: VirtualRegs,
}

// =============================================================================
// Alt allocator: external interface

struct Context<'a> {
    // Set of registers that the allocator can use.
    usable_regs: RegisterSet,

    // Current instruction as well as reference to function and ISA.
    cur: EncCursor<'a>,

    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    encinfo: EncInfo,

    // References to contextual data structures we need.
    domtree: &'a mut DominatorTree,
    topo: &'a mut TopoOrder,

    // The running state.
    state: &'a mut AAState,
}

// Counter of number of runs so far
static RUNS_SO_FAR: AtomicUsize = AtomicUsize::new(0);

// Get display limits, if any, from AA_NOTBELOW and AA_NOTABOVE
fn get_limits() -> (Option<usize>, Option<usize>) {
    let limit_notbelow = match env::var("AA_NOTBELOW") {
        Ok(ref s) => Some(
            s.parse::<usize>()
                .expect("Usage: set AA_NOTBELOW=<integer>"),
        ),
        _ => None,
    };
    let limit_notabove = match env::var("AA_NOTABOVE") {
        Ok(ref s) => Some(
            s.parse::<usize>()
                .expect("Usage: set AA_NOTABOVE=<integer>"),
        ),
        _ => None,
    };
    (limit_notbelow, limit_notabove)
}

type ValueList = EntityList<Value>;

struct VirtualRegs {
    /// The primary table of virtual registers.
    vregs: PrimaryMap<VirtReg, ValueList>,
    value_vreg: SecondaryMap<Value, Option<VirtReg>>,
    value_pool: ListPool<Value>,

    ebb_params_vreg: SecondaryMap<Ebb, Option<Vec<VirtReg>>>,
}

impl VirtualRegs {
    fn new() -> Self {
        Self {
            vregs: PrimaryMap::new(),
            value_vreg: SecondaryMap::new(),
            value_pool: ListPool::new(),
            ebb_params_vreg: SecondaryMap::new(),
        }
    }

    fn clear(&mut self) {
        self.vregs.clear();
        self.value_vreg.clear();
        self.value_pool.clear();
        self.ebb_params_vreg.clear();
    }
}

/// Make phis explicit: replace each block-terminating jump with params, with a parallel assignment
/// followed by the same jump without params.
///
/// Initially, generate a naive sequentialisation of the parallel assignment just by copying
/// through a fresh set of vregs.
impl<'a> Context<'a> {
    fn make_phis_explicit(&mut self) {
        self.topo.reset(self.cur.func.layout.ebbs());

        let vregs = &mut self.state.vregs;

        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            // Step 1: assign virtual reg to the ebb parameters.
            if let Some(ebb_vregs) = &vregs.ebb_params_vreg[ebb] {
                // If it's already been visited, all the vregs must have been preallocated.
                debug_assert!(ebb_vregs.len() == self.cur.func.dfg.num_ebb_params(ebb));
            } else {
                // This block hasn't ever been visited, allocate vregs.
                let mut ebb_vregs = Vec::with_capacity(self.cur.func.dfg.num_ebb_params(ebb));
                for &ebb_param in self.cur.func.dfg.ebb_params(ebb) {
                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                    vregs.value_vreg[ebb_param] = Some(vreg);
                    info!("{:?}: param {} has vreg {}", ebb, ebb_param, vreg);
                    ebb_vregs.push(vreg);
                }
                vregs.ebb_params_vreg[ebb] = Some(ebb_vregs);
            }

            // Step 2: assign values to instructions.
            // - if it's a control flow instruction, then rewrite it.
            // - otherwise, one vreg for each result produced.

            // First collect the vreg copies that are needed in this step, and add them to the IR
            // graph later, to avoid borrowing the IR graph while we're iterating on it.
            let mut vreg_copies_to_insert = Vec::new();

            for inst in self.cur.func.layout.ebb_insts(ebb) {
                // Sanity check: every value mentioned in the instruction has been assigned a
                // virtual register.
                for &input in self.cur.func.dfg.inst_args(inst) {
                    debug_assert!(
                        vregs.value_vreg[input].is_some(),
                        "missing vreg for an inst's input"
                    );
                }

                // Assign a virtual register to every result.
                for &result in self.cur.func.dfg.inst_results(inst) {
                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(result, &mut vregs.value_pool);
                    debug_assert!(
                        vregs.value_vreg[result].is_none(),
                        "ssa value assigned twice"
                    );
                    vregs.value_vreg[result] = Some(vreg);
                    info!("{:?}: inst result {} has vreg {}", ebb, result, vreg);
                }

                if self.cur.func.dfg[inst].opcode().is_branch() {
                    let target = match self.cur.func.dfg[inst] {
                        InstructionData::Branch { destination, .. }
                        | InstructionData::BranchIcmp { destination, .. }
                        | InstructionData::BranchInt { destination, .. }
                        | InstructionData::BranchFloat { destination, .. }
                        | InstructionData::BranchTable { destination, .. }
                        | InstructionData::Jump { destination, .. } => destination,
                        _ => panic!("Unexpected branch format in make_phis_explicit"),
                    };

                    // Make sure that the target EBBs has virtual regs.
                    if vregs.ebb_params_vreg[target].is_none() {
                        // TODO common this out in a small helper function.
                        // This block hasn't ever been visited, allocate vregs.
                        let mut ebb_vregs =
                            Vec::with_capacity(self.cur.func.dfg.num_ebb_params(target));
                        for &ebb_param in self.cur.func.dfg.ebb_params(target) {
                            let vreg = vregs.vregs.push(ValueList::new());
                            vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                            vregs.value_vreg[ebb_param] = Some(vreg);
                            ebb_vregs.push(vreg);
                            info!("{:?}: param {} has vreg {}", ebb, ebb_param, vreg);
                        }
                        vregs.ebb_params_vreg[target] = Some(ebb_vregs);
                    }

                    // Introduce a parallel copy for every single EBB param.
                    let ebb_vregs = vregs.ebb_params_vreg[target].as_ref().unwrap();
                    debug_assert!(
                        self.cur.func.dfg.inst_variable_args(inst).len() == ebb_vregs.len()
                    );

                    // Because of branch splitting, only terminator instructions can have branch
                    // parameters.
                    debug_assert!(
                        self.cur.func.dfg[inst].opcode().is_terminator()
                            || self.cur.func.dfg.inst_variable_args(inst).len() == 0
                    );

                    if self.cur.func.dfg.inst_variable_args(inst).len() == 0 {
                        continue;
                    }

                    for (&param, &ebb_vreg) in self
                        .cur
                        .func
                        .dfg
                        .inst_variable_args(inst)
                        .iter()
                        .zip(ebb_vregs.iter())
                    {
                        let source_vreg =
                            vregs.value_vreg[param].expect("branch param has no vreg");
                        vreg_copies_to_insert.push((source_vreg, ebb_vreg));
                    }

                    // Keep only branch arguments, remove passed variables.
                    {
                        // TODO see if we could factor this code out in the valuelist impl, or at
                        // least in the dfg.
                        let dfg = &mut self.cur.func.dfg;
                        let branch_args = dfg[inst]
                            .take_value_list()
                            .expect("branch params")
                            .as_slice(&dfg.value_lists)
                            .iter()
                            .copied()
                            .collect::<Vec<_>>();
                        let (new_branch_args, _) = branch_args
                            .split_at(dfg[inst].opcode().constraints().num_fixed_value_arguments());
                        let new_branch_value_list =
                            ValueList::from_slice(new_branch_args, &mut dfg.value_lists);
                        dfg[inst].put_value_list(new_branch_value_list);
                    }
                }
            }

            // Actually add the vreg copies.
            self.cur.goto_last_inst(ebb);
            for (source_vreg, dest_vreg) in vreg_copies_to_insert {
                info!("{:?}: copy_vreg {} -> {}", ebb, source_vreg, dest_vreg);
                // TODO ins() requires the instruction to be encodable, and this isn't the case for
                // copy_vreg, which is just there for regalloc purposes, so we might need to use a
                // different way to represent it.
                //self.cur.ins().copy_vreg(source_vreg, dest_vreg);
            }
        }

        // Eventually, remove branch parameters for all the blocks. Do this after iterating over
        // all the blocks, to make sure we don't lose information related to branch going in either
        // direction.
        for ebb in self.cur.func.layout.ebbs() {
            let ebb_params = self
                .cur
                .func
                .dfg
                .ebb_params(ebb)
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            for ebb_param in ebb_params {
                self.cur.func.dfg.swap_remove_ebb_param(ebb_param);
            }
        }
    }
}

impl AAState {
    /// Create a new alt allocator state.
    pub fn new() -> Self {
        Self {
            vregs: VirtualRegs::new(),
        }
    }

    /// Clear the state of the allocator.
    pub fn clear(&mut self) {
        self.vregs.clear();
    }

    /// Run register allocation.
    pub fn run(
        &mut self,
        isa: &dyn TargetIsa,
        func: &mut Function,
        cfg: &mut ControlFlowGraph,
        domtree: &mut DominatorTree,
        topo: &mut TopoOrder,
    ) {
        let run_number = 1 + RUNS_SO_FAR.fetch_add(1, Ordering::SeqCst);
        let limits = get_limits();

        let mut ctx = Context {
            usable_regs: isa.allocatable_registers(func),
            cur: EncCursor::new(func, isa),
            encinfo: isa.encoding_info(),
            domtree,
            topo,
            state: self,
        };
        ctx.show(limits, run_number, "Incoming");

        branch_splitting::run(isa, ctx.cur.func, cfg, ctx.domtree, ctx.topo);
        ctx.show(limits, run_number, "After branch splitting");

        ctx.make_phis_explicit();
        ctx.show(limits, run_number, "After making phis explicit");

        unimplemented!("computing live values");

        //let r = ctx.run_minimal_allocator();
        //ctx.show(limits, run_number, "Completed");

        //r
    }
}