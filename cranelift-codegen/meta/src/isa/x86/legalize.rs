use crate::cdsl::ast::{var, ExprBuilder, Literal};
use crate::cdsl::instructions::InstructionGroup;
use crate::cdsl::xform::TransformGroupBuilder;

use crate::shared::types::Int::{I32, I64};
use crate::shared::Definitions as SharedDefinitions;

pub fn define(shared: &mut SharedDefinitions, x86_instructions: &InstructionGroup) {
    let mut group = TransformGroupBuilder::new(
        "x86_expand",
        r#"
    Legalize instructions by expansion.

    Use x86-specific instructions if needed."#,
    )
    .isa("x86")
    .chain_with(shared.transform_groups.by_name("expand_flags").id);

    // List of instructions.
    let insts = &shared.instructions;
    let band = insts.by_name("band");
    let bitcast = insts.by_name("bitcast");
    let bor = insts.by_name("bor");
    let clz = insts.by_name("clz");
    let ctz = insts.by_name("ctz");
    let fcmp = insts.by_name("fcmp");
    let fcvt_from_uint = insts.by_name("fcvt_from_uint");
    let fcvt_to_sint = insts.by_name("fcvt_to_sint");
    let fcvt_to_uint = insts.by_name("fcvt_to_uint");
    let fcvt_to_sint_sat = insts.by_name("fcvt_to_sint_sat");
    let fcvt_to_uint_sat = insts.by_name("fcvt_to_uint_sat");
    let fmax = insts.by_name("fmax");
    let fmin = insts.by_name("fmin");
    let iadd = insts.by_name("iadd");
    let iconst = insts.by_name("iconst");
    let imul = insts.by_name("imul");
    let isub = insts.by_name("isub");
    let popcnt = insts.by_name("popcnt");
    let sdiv = insts.by_name("sdiv");
    let selectif = insts.by_name("selectif");
    let smulhi = insts.by_name("smulhi");
    let splat = insts.by_name("splat");
    let srem = insts.by_name("srem");
    let udiv = insts.by_name("udiv");
    let umulhi = insts.by_name("umulhi");
    let ushr_imm = insts.by_name("ushr_imm");
    let urem = insts.by_name("urem");

    let x86_bsf = x86_instructions.by_name("x86_bsf");
    let x86_bsr = x86_instructions.by_name("x86_bsr");
    let x86_pshuf = x86_instructions.by_name("x86_pshuf");
    let x86_umulx = x86_instructions.by_name("x86_umulx");
    let x86_smulx = x86_instructions.by_name("x86_smulx");

    // List of immediates.
    let floatcc = shared.operand_kinds.by_name("floatcc");
    let imm64 = shared.operand_kinds.by_name("imm64");
    let intcc = shared.operand_kinds.by_name("intcc");

    // Division and remainder.
    //
    // The srem expansion requires custom code because srem INT_MIN, -1 is not
    // allowed to trap. The other ops need to check avoid_div_traps.
    group.custom_legalize(sdiv, "expand_sdivrem");
    group.custom_legalize(srem, "expand_sdivrem");
    group.custom_legalize(udiv, "expand_udivrem");
    group.custom_legalize(urem, "expand_udivrem");

    // Double length (widening) multiplication.
    let a = var("a");
    let x = var("x");
    let y = var("y");
    let a1 = var("a1");
    let a2 = var("a2");
    let res_lo = var("res_lo");
    let res_hi = var("res_hi");

    group.legalize(
        def!(res_hi = umulhi(x, y)),
        vec![def!((res_lo, res_hi) = x86_umulx(x, y))],
    );

    group.legalize(
        def!(res_hi = smulhi(x, y)),
        vec![def!((res_lo, res_hi) = x86_smulx(x, y))],
    );

    // SIMD
//    group.legalize(
//        def!(y = splat(x)),
//        vec![
//            def!(a = bitcast(x)),
//            def!(y = x86_pshuf(a))
//        ]
//    );

    // Floating point condition codes.
    //
    // The 8 condition codes in `supported_floatccs` are directly supported by a
    // `ucomiss` or `ucomisd` instruction. The remaining codes need legalization
    // patterns.

    let floatcc_eq = Literal::enumerator_for(floatcc, "eq");
    let floatcc_ord = Literal::enumerator_for(floatcc, "ord");
    let floatcc_ueq = Literal::enumerator_for(floatcc, "ueq");
    let floatcc_ne = Literal::enumerator_for(floatcc, "ne");
    let floatcc_uno = Literal::enumerator_for(floatcc, "uno");
    let floatcc_one = Literal::enumerator_for(floatcc, "one");

    // Equality needs an explicit `ord` test which checks the parity bit.
    group.legalize(
        def!(a = fcmp(floatcc_eq, x, y)),
        vec![
            def!(a1 = fcmp(floatcc_ord, x, y)),
            def!(a2 = fcmp(floatcc_ueq, x, y)),
            def!(a = band(a1, a2)),
        ],
    );
    group.legalize(
        def!(a = fcmp(floatcc_ne, x, y)),
        vec![
            def!(a1 = fcmp(floatcc_uno, x, y)),
            def!(a2 = fcmp(floatcc_one, x, y)),
            def!(a = bor(a1, a2)),
        ],
    );

    let floatcc_lt = &Literal::enumerator_for(floatcc, "lt");
    let floatcc_gt = &Literal::enumerator_for(floatcc, "gt");
    let floatcc_le = &Literal::enumerator_for(floatcc, "le");
    let floatcc_ge = &Literal::enumerator_for(floatcc, "ge");
    let floatcc_ugt = &Literal::enumerator_for(floatcc, "ugt");
    let floatcc_ult = &Literal::enumerator_for(floatcc, "ult");
    let floatcc_uge = &Literal::enumerator_for(floatcc, "uge");
    let floatcc_ule = &Literal::enumerator_for(floatcc, "ule");

    // Inequalities that need to be reversed.
    for &(cc, rev_cc) in &[
        (floatcc_lt, floatcc_gt),
        (floatcc_le, floatcc_ge),
        (floatcc_ugt, floatcc_ult),
        (floatcc_uge, floatcc_ule),
    ] {
        group.legalize(def!(a = fcmp(cc, x, y)), vec![def!(a = fcmp(rev_cc, y, x))]);
    }

    // We need to modify the CFG for min/max legalization.
    group.custom_legalize(fmin, "expand_minmax");
    group.custom_legalize(fmax, "expand_minmax");

    // Conversions from unsigned need special handling.
    group.custom_legalize(fcvt_from_uint, "expand_fcvt_from_uint");
    // Conversions from float to int can trap and modify the control flow graph.
    group.custom_legalize(fcvt_to_sint, "expand_fcvt_to_sint");
    group.custom_legalize(fcvt_to_uint, "expand_fcvt_to_uint");
    group.custom_legalize(fcvt_to_sint_sat, "expand_fcvt_to_sint_sat");
    group.custom_legalize(fcvt_to_uint_sat, "expand_fcvt_to_uint_sat");

    // Count leading and trailing zeroes, for baseline x86_64
    let c_minus_one = var("c_minus_one");
    let c_thirty_one = var("c_thirty_one");
    let c_thirty_two = var("c_thirty_two");
    let c_sixty_three = var("c_sixty_three");
    let c_sixty_four = var("c_sixty_four");
    let index1 = var("index1");
    let r2flags = var("r2flags");
    let index2 = var("index2");

    let intcc_eq = Literal::enumerator_for(intcc, "eq");
    let imm64_minus_one = Literal::constant(imm64, -1);
    let imm64_63 = Literal::constant(imm64, 63);
    group.legalize(
        def!(a = clz.I64(x)),
        vec![
            def!(c_minus_one = iconst(imm64_minus_one)),
            def!(c_sixty_three = iconst(imm64_63)),
            def!((index1, r2flags) = x86_bsr(x)),
            def!(index2 = selectif(intcc_eq, r2flags, c_minus_one, index1)),
            def!(a = isub(c_sixty_three, index2)),
        ],
    );

    let imm64_31 = Literal::constant(imm64, 31);
    group.legalize(
        def!(a = clz.I32(x)),
        vec![
            def!(c_minus_one = iconst(imm64_minus_one)),
            def!(c_thirty_one = iconst(imm64_31)),
            def!((index1, r2flags) = x86_bsr(x)),
            def!(index2 = selectif(intcc_eq, r2flags, c_minus_one, index1)),
            def!(a = isub(c_thirty_one, index2)),
        ],
    );

    let imm64_64 = Literal::constant(imm64, 64);
    group.legalize(
        def!(a = ctz.I64(x)),
        vec![
            def!(c_sixty_four = iconst(imm64_64)),
            def!((index1, r2flags) = x86_bsf(x)),
            def!(a = selectif(intcc_eq, r2flags, c_sixty_four, index1)),
        ],
    );

    let imm64_32 = Literal::constant(imm64, 32);
    group.legalize(
        def!(a = ctz.I32(x)),
        vec![
            def!(c_thirty_two = iconst(imm64_32)),
            def!((index1, r2flags) = x86_bsf(x)),
            def!(a = selectif(intcc_eq, r2flags, c_thirty_two, index1)),
        ],
    );

    // Population count for baseline x86_64
    let qv1 = var("qv1");
    let qv3 = var("qv3");
    let qv4 = var("qv4");
    let qv5 = var("qv5");
    let qv6 = var("qv6");
    let qv7 = var("qv7");
    let qv8 = var("qv8");
    let qv9 = var("qv9");
    let qv10 = var("qv10");
    let qv11 = var("qv11");
    let qv12 = var("qv12");
    let qv13 = var("qv13");
    let qv14 = var("qv14");
    let qv15 = var("qv15");
    let qv16 = var("qv16");
    let qc77 = var("qc77");
    #[allow(non_snake_case)]
    let qc0F = var("qc0F");
    let qc01 = var("qc01");

    let imm64_1 = Literal::constant(imm64, 1);
    let imm64_4 = Literal::constant(imm64, 4);
    group.legalize(
        def!(qv16 = popcnt.I64(qv1)),
        vec![
            def!(qv3 = ushr_imm(qv1, imm64_1)),
            def!(qc77 = iconst(Literal::constant(imm64, 0x7777777777777777))),
            def!(qv4 = band(qv3, qc77)),
            def!(qv5 = isub(qv1, qv4)),
            def!(qv6 = ushr_imm(qv4, imm64_1)),
            def!(qv7 = band(qv6, qc77)),
            def!(qv8 = isub(qv5, qv7)),
            def!(qv9 = ushr_imm(qv7, imm64_1)),
            def!(qv10 = band(qv9, qc77)),
            def!(qv11 = isub(qv8, qv10)),
            def!(qv12 = ushr_imm(qv11, imm64_4)),
            def!(qv13 = iadd(qv11, qv12)),
            def!(qc0F = iconst(Literal::constant(imm64, 0x0F0F0F0F0F0F0F0F))),
            def!(qv14 = band(qv13, qc0F)),
            def!(qc01 = iconst(Literal::constant(imm64, 0x0101010101010101))),
            def!(qv15 = imul(qv14, qc01)),
            def!(qv16 = ushr_imm(qv15, Literal::constant(imm64, 56))),
        ],
    );

    let lv1 = var("lv1");
    let lv3 = var("lv3");
    let lv4 = var("lv4");
    let lv5 = var("lv5");
    let lv6 = var("lv6");
    let lv7 = var("lv7");
    let lv8 = var("lv8");
    let lv9 = var("lv9");
    let lv10 = var("lv10");
    let lv11 = var("lv11");
    let lv12 = var("lv12");
    let lv13 = var("lv13");
    let lv14 = var("lv14");
    let lv15 = var("lv15");
    let lv16 = var("lv16");
    let lc77 = var("lc77");
    #[allow(non_snake_case)]
    let lc0F = var("lc0F");
    let lc01 = var("lc01");

    group.legalize(
        def!(lv16 = popcnt.I32(lv1)),
        vec![
            def!(lv3 = ushr_imm(lv1, imm64_1)),
            def!(lc77 = iconst(Literal::constant(imm64, 0x77777777))),
            def!(lv4 = band(lv3, lc77)),
            def!(lv5 = isub(lv1, lv4)),
            def!(lv6 = ushr_imm(lv4, imm64_1)),
            def!(lv7 = band(lv6, lc77)),
            def!(lv8 = isub(lv5, lv7)),
            def!(lv9 = ushr_imm(lv7, imm64_1)),
            def!(lv10 = band(lv9, lc77)),
            def!(lv11 = isub(lv8, lv10)),
            def!(lv12 = ushr_imm(lv11, imm64_4)),
            def!(lv13 = iadd(lv11, lv12)),
            def!(lc0F = iconst(Literal::constant(imm64, 0x0F0F0F0F))),
            def!(lv14 = band(lv13, lc0F)),
            def!(lc01 = iconst(Literal::constant(imm64, 0x01010101))),
            def!(lv15 = imul(lv14, lc01)),
            def!(lv16 = ushr_imm(lv15, Literal::constant(imm64, 24))),
        ],
    );

    group.build_and_add_to(&mut shared.transform_groups);
}
