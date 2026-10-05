//! TGSI shader program text for the compositor's VirGL pipeline.
//!
//! The renderer owns object creation and per-draw constants; this module owns only the
//! generated vertex/fragment program text and the Gaussian kernel those programs embed.

use super::CONSTANTS_PER_CLIP_MASK;

pub(super) const GAUSSIAN_PAIR_OFFSETS: [f32; 3] = [1.407_333_4, 3.294_215, 5.201_813];
pub(super) const GAUSSIAN_WEIGHTS: [f32; 4] = [0.297_322_5, 0.091_848_34, 0.010_991_33, 0.199_675_63];

pub(super) const VERTEX_SHADER_SOURCE: &str = "VERT\n\
DCL IN[0]\n\
DCL IN[1]\n\
DCL OUT[0], POSITION\n\
DCL OUT[1], GENERIC[0]\n\
DCL OUT[2], GENERIC[1]\n\
DCL CONST[0][0..2]\n\
IMM[0] FLT32 {0.0, 0.0, 0.0, 1.0}\n\
0: MAD OUT[0].xy, IN[0], CONST[0][0].zwzw, CONST[0][0].xyxy\n\
1: MOV OUT[0].zw, IMM[0].zzzw\n\
2: MAD OUT[1].xy, IN[1], CONST[0][2].zwzw, CONST[0][2].xyxy\n\
3: MOV OUT[1].zw, IMM[0].zzzw\n\
4: MAD OUT[2].xy, IN[0], CONST[0][1].zwzw, CONST[0][1].xyxy\n\
5: MOV OUT[2].zw, IMM[0].zzzw\n\
6: END\n";

fn append_rounded_rect_sdf(
    shader: &mut String,
    instruction: &mut usize,
    parameter_constant: usize,
    original: bool,
) {
    use std::fmt::Write as _;

    let mut emit = |line: &str| {
        writeln!(shader, "{}: {line}", *instruction).unwrap();
        *instruction += 1;
    };
    if original {
        emit(&format!(
            "ADD TEMP[2].xy, CONST[0][{parameter_constant}].xyxy, -CONST[0][{}].zwzw",
            parameter_constant + 3
        ));
        emit(&format!(
            "ADD TEMP[2].xy, TEMP[2].xyxy, CONST[0][{}].yyyy",
            parameter_constant + 3
        ));
        emit(&format!(
            "ADD TEMP[2].zw, CONST[0][{parameter_constant}].zwzw, -CONST[0][{}].zwzw",
            parameter_constant + 3
        ));
        emit(&format!(
            "ADD TEMP[2].zw, TEMP[2].zwzw, -CONST[0][{}].yyyy",
            parameter_constant + 3
        ));
        emit("ADD TEMP[4].xy, TEMP[2].xyxy, TEMP[2].zwzw");
    } else {
        emit(&format!(
            "ADD TEMP[4].xy, CONST[0][{parameter_constant}].xyxy, CONST[0][{parameter_constant}].zwzw"
        ));
    }
    emit("MUL TEMP[4].xy, TEMP[4].xyxy, IMM[0].xxxx");
    if original {
        emit("ADD TEMP[4].zw, TEMP[2].zwzw, -TEMP[2].xyxy");
    } else {
        emit(&format!(
            "ADD TEMP[4].zw, CONST[0][{parameter_constant}].zwzw, -CONST[0][{parameter_constant}].xyxy"
        ));
    }
    emit("MUL TEMP[4].zw, TEMP[4].zwzw, IMM[0].xxxx");
    emit("SGE TEMP[7].y, IN[1].xxxx, TEMP[4].xxxx");
    emit("SGE TEMP[7].z, IN[1].yyyy, TEMP[4].yyyy");
    emit("IF TEMP[7].zzzz");
    emit("IF TEMP[7].yyyy");
    emit(&format!(
        "MOV TEMP[5].z, CONST[0][{}].zzzz",
        parameter_constant + 1
    ));
    emit(&format!(
        "MOV TEMP[5].w, CONST[0][{}].zzzz",
        parameter_constant + 2
    ));
    emit("ELSE");
    emit(&format!(
        "MOV TEMP[5].z, CONST[0][{}].wwww",
        parameter_constant + 1
    ));
    emit(&format!(
        "MOV TEMP[5].w, CONST[0][{}].wwww",
        parameter_constant + 2
    ));
    emit("ENDIF");
    emit("ELSE");
    emit("IF TEMP[7].yyyy");
    emit(&format!(
        "MOV TEMP[5].z, CONST[0][{}].yyyy",
        parameter_constant + 1
    ));
    emit(&format!(
        "MOV TEMP[5].w, CONST[0][{}].yyyy",
        parameter_constant + 2
    ));
    emit("ELSE");
    emit(&format!(
        "MOV TEMP[5].z, CONST[0][{}].xxxx",
        parameter_constant + 1
    ));
    emit(&format!(
        "MOV TEMP[5].w, CONST[0][{}].xxxx",
        parameter_constant + 2
    ));
    emit("ENDIF");
    emit("ENDIF");
    if original {
        emit(&format!(
            "ADD TEMP[5].zw, TEMP[5].zwzw, -CONST[0][{}].yyyy",
            parameter_constant + 3
        ));
        emit("MAX TEMP[5].zw, TEMP[5].zwzw, IMM[0].zzzz");
    }
    emit("MAX TEMP[5].zw, TEMP[5].zwzw, IMM[0].yyyy");
    emit("ADD TEMP[5].xy, IN[1].xyxy, -TEMP[4].xyxy");
    emit("ABS TEMP[5].xy, TEMP[5].xyxy");
    emit("ADD TEMP[5].xy, TEMP[5].xyxy, -TEMP[4].zwzw");
    emit("ADD TEMP[5].xy, TEMP[5].xyxy, TEMP[5].zwzw");
    emit("MAX TEMP[4].xy, TEMP[5].xyxy, IMM[0].zzzz");
    emit("RCP TEMP[4].zw, TEMP[5].zwzw");
    emit("MUL TEMP[4].xy, TEMP[4].xyxy, TEMP[4].zwzw");
    emit("DP2 TEMP[6].x, TEMP[4], TEMP[4]");
    emit("SQRT TEMP[6].x, TEMP[6].xxxx");
    emit("MIN TEMP[4].y, TEMP[5].zzzz, TEMP[5].wwww");
    emit("MUL TEMP[6].x, TEMP[6].xxxx, TEMP[4].yyyy");
    emit("MAX TEMP[4].x, TEMP[5].xxxx, TEMP[5].yyyy");
    emit("MIN TEMP[4].x, TEMP[4].xxxx, IMM[0].zzzz");
    emit("ADD TEMP[6].x, TEMP[6].xxxx, TEMP[4].xxxx");
    emit("ADD TEMP[6].x, TEMP[6].xxxx, -TEMP[4].yyyy");
}

pub(super) fn fragment_shader(clip_masks: usize) -> String {
    use std::fmt::Write as _;

    let [near_offset, middle_offset, far_offset] = GAUSSIAN_PAIR_OFFSETS;
    let [near_weight, middle_weight, far_weight, center_weight] = GAUSSIAN_WEIGHTS;
    let color_constant = clip_masks * CONSTANTS_PER_CLIP_MASK;
    let mode_constant = color_constant + 1;
    let parameter_constant = mode_constant + 1;
    let last_fragment_constant = parameter_constant + 3;
    let mut shader = String::from(&format!(
        "FRAG\n\
DCL IN[0], GENERIC[0], LINEAR\n\
DCL IN[1], GENERIC[1], LINEAR\n\
DCL OUT[0], COLOR\n\
DCL SAMP[0]\n\
DCL SVIEW[0], 2D, FLOAT\n\
DCL TEMP[0..7]\n\
DCL CONST[0][0..{last_fragment_constant}]\n\
IMM[0] FLT32 {{0.5, 1.0, 0.0, 0.11111111}}\n\
IMM[1] FLT32 {{1.0, 2.0, 3.0, 4.0}}\n\
IMM[2] FLT32 {{-1.0, -1.0, 0.0, 0.0}}\n\
IMM[3] FLT32 {{0.0, -1.0, 0.0, 0.0}}\n\
IMM[4] FLT32 {{1.0, -1.0, 0.0, 0.0}}\n\
IMM[5] FLT32 {{-1.0, 0.0, 0.0, 0.0}}\n\
IMM[6] FLT32 {{0.0, 0.0, 0.0, 0.0}}\n\
IMM[7] FLT32 {{1.0, 0.0, 0.0, 0.0}}\n\
IMM[8] FLT32 {{-1.0, 1.0, 0.0, 0.0}}\n\
IMM[9] FLT32 {{0.0, 1.0, 0.0, 0.0}}\n\
IMM[10] FLT32 {{1.0, 1.0, 0.0, 0.0}}\n\
IMM[11] FLT32 {{5.0, 0.0, 0.0, 0.0}}\n\
IMM[12] FLT32 {{0.0, 1.0, 2.0, 3.0}}\n\
IMM[13] FLT32 {{4.0, 5.0, 6.0, 7.0}}\n\
IMM[14] FLT32 {{8.0, 9.0, 10.0, 11.0}}\n\
IMM[15] FLT32 {{12.0, 13.0, 14.0, 15.0}}\n\
IMM[16] FLT32 {{16.0, 17.0, 18.0, 19.0}}\n\
IMM[17] FLT32 {{-{far_offset}, -{middle_offset}, -{near_offset}, 0.0}}\n\
IMM[18] FLT32 {{{near_offset}, {middle_offset}, {far_offset}, 0.0}}\n\
IMM[19] FLT32 {{{far_weight}, {middle_weight}, {near_weight}, {center_weight}}}\n"
    ));
    let mut instruction = 0;
    writeln!(shader, "{instruction}: MOV TEMP[3], IMM[0].yyyy").unwrap();
    instruction += 1;
    for mask in 0..clip_masks {
        let base = mask * CONSTANTS_PER_CLIP_MASK;
        let edges = base + 5;
        let immediate = 12 + mask / 4;
        let component = ["xxxx", "yyyy", "zzzz", "wwww"][mask % 4];
        writeln!(
            shader,
            "{instruction}: SLT TEMP[7].w, IMM[{immediate}].{component}, CONST[0][{mode_constant}].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(shader, "{instruction}: IF TEMP[7].wwww").unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SGE TEMP[1].x, IN[1].xxxx, CONST[0][{edges}].xxxx"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SLT TEMP[1].y, IN[1].xxxx, CONST[0][{edges}].zzzz"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SGE TEMP[1].y, IN[1].yyyy, CONST[0][{edges}].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SLT TEMP[1].y, IN[1].yyyy, CONST[0][{edges}].wwww"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[3].x, TEMP[3].xxxx, TEMP[1].xxxx"
        )
        .unwrap();
        instruction += 1;
        for corner in 0..4 {
            let x_comparison = if corner == 0 || corner == 3 {
                format!(
                    "SLT TEMP[1].x, IN[1].xxxx, CONST[0][{}].xxxx",
                    base + corner
                )
            } else {
                format!(
                    "SLT TEMP[1].x, CONST[0][{}].xxxx, IN[1].xxxx",
                    base + corner
                )
            };
            let y_comparison = if corner <= 1 {
                format!(
                    "SLT TEMP[1].y, IN[1].yyyy, CONST[0][{}].yyyy",
                    base + corner
                )
            } else {
                format!(
                    "SLT TEMP[1].y, CONST[0][{}].yyyy, IN[1].yyyy",
                    base + corner
                )
            };
            writeln!(shader, "{instruction}: {x_comparison}").unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: {y_comparison}").unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: IF TEMP[1].xxxx").unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: ADD TEMP[2].xy, IN[1].xyxy, -CONST[0][{}].xyxy",
                base + corner
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MUL TEMP[2].xy, TEMP[2].xyxy, CONST[0][{}].zwzw",
                base + corner
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: DP2 TEMP[2].x, TEMP[2], TEMP[2]").unwrap();
            instruction += 1;
            let component = ["xxxx", "yyyy", "zzzz", "wwww"][corner];
            writeln!(
                shader,
                "{instruction}: ADD TEMP[2].x, IMM[0].yyyy, -TEMP[2].xxxx"
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MAD TEMP[2].x, TEMP[2].xxxx, CONST[0][{}].{}, IMM[0].xxxx",
                base + 4,
                component
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MIN TEMP[3].x, TEMP[3].xxxx, TEMP[2].xxxx"
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: ENDIF").unwrap();
            instruction += 1;
        }
        writeln!(shader, "{instruction}: ENDIF").unwrap();
        instruction += 1;
    }
    writeln!(
        shader,
        "{instruction}: MAX TEMP[3].x, TEMP[3].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{mode_constant}].xxxx, IMM[11].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    append_rounded_rect_sdf(&mut shader, &mut instruction, parameter_constant, false);
    writeln!(
        shader,
        "{instruction}: ABS TEMP[4].x, CONST[0][{}].xxxx",
        parameter_constant + 3
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAD TEMP[6].x, -TEMP[6].xxxx, TEMP[4].xxxx, IMM[0].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[6].x, TEMP[6].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MIN TEMP[6].x, TEMP[6].xxxx, IMM[0].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[6].y, TEMP[6].xxxx, TEMP[6].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAD TEMP[4].x, -TEMP[6].xxxx, IMM[1].yyyy, IMM[1].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[6].x, TEMP[6].yyyy, TEMP[4].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SLT TEMP[7].x, CONST[0][{}].xxxx, IMM[0].zzzz",
        parameter_constant + 3
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[6].x, IMM[0].yyyy, -TEMP[6].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MOV TEMP[1].w, TEMP[6].xxxx").unwrap();
    instruction += 1;
    append_rounded_rect_sdf(&mut shader, &mut instruction, parameter_constant, true);
    writeln!(
        shader,
        "{instruction}: ADD TEMP[6].x, TEMP[6].xxxx, IMM[0].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[6].x, TEMP[6].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MIN TEMP[6].x, TEMP[6].xxxx, IMM[0].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[6].x, TEMP[1].wwww, TEMP[6].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], CONST[0][{color_constant}], TEMP[6].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{mode_constant}].xxxx, IMM[1].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ADD TEMP[4].xy, CONST[0][{parameter_constant}].zwzw, -CONST[0][{parameter_constant}].xyxy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].xy, IN[1].xyxy, -CONST[0][{parameter_constant}].xyxy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: DP2 TEMP[5].x, TEMP[5], TEMP[4]").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: DP2 TEMP[5].y, TEMP[4], TEMP[4]").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: RCP TEMP[5].y, TEMP[5].yyyy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[5].x, TEMP[5].xxxx, TEMP[5].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MOV TEMP[5].z, TEMP[5].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].y, CONST[0][{}].zzzz, TEMP[5].zzzz",
        parameter_constant + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SLT TEMP[7].z, CONST[0][{}].wwww, TEMP[5].zzzz",
        parameter_constant + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[7].y, TEMP[7].yyyy, TEMP[7].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[7].y, IMM[0].yyyy, -TEMP[7].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].x, TEMP[5].xxxx, -CONST[0][{}].xxxx",
        parameter_constant + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].y, CONST[0][{}].yyyy, -CONST[0][{}].xxxx",
        parameter_constant + 1,
        parameter_constant + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: RCP TEMP[5].y, TEMP[5].yyyy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[5].x, TEMP[5].xxxx, TEMP[5].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[5].x, TEMP[5].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MIN TEMP[5].x, TEMP[5].xxxx, IMM[0].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: LRP TEMP[0], TEMP[5].xxxx, CONST[0][{}], CONST[0][{}]",
        parameter_constant + 3,
        parameter_constant + 2
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MUL TEMP[0], TEMP[0], TEMP[7].yyyy").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].x, CONST[0][{mode_constant}].xxxx, IMM[1].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    for (sample, (offset, weight)) in [
        ("IMM[17].xxxx", "IMM[19].xxxx"),
        ("IMM[17].yyyy", "IMM[19].yyyy"),
        ("IMM[17].zzzz", "IMM[19].zzzz"),
        ("IMM[0].zzzz", "IMM[19].wwww"),
        ("IMM[18].xxxx", "IMM[19].zzzz"),
        ("IMM[18].yyyy", "IMM[19].yyyy"),
        ("IMM[18].zzzz", "IMM[19].xxxx"),
    ]
    .into_iter()
    .enumerate()
    {
        writeln!(shader, "{instruction}: MAD TEMP[6].xy, CONST[0][{parameter_constant}].xyxy, {offset}, IN[0].xyxy").unwrap();
        instruction += 1;
        writeln!(shader, "{instruction}: TEX TEMP[4], TEMP[6], SAMP[0], 2D").unwrap();
        instruction += 1;
        let operation = if sample == 0 { "MUL" } else { "MAD" };
        let tail = if sample == 0 { "" } else { ", TEMP[0]" };
        writeln!(
            shader,
            "{instruction}: {operation} TEMP[0], TEMP[4], {weight}{tail}"
        )
        .unwrap();
        instruction += 1;
    }
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].x, CONST[0][{mode_constant}].xxxx, IMM[1].wwww"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], CONST[0][{color_constant}], TEMP[0].wwww"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], TEMP[0], CONST[0][{color_constant}]"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: TEX TEMP[0], IN[0], SAMP[0], 2D").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{mode_constant}].xxxx, IMM[1].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], CONST[0][{color_constant}], TEMP[0].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], TEMP[0], CONST[0][{color_constant}]"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MUL OUT[0], TEMP[0], TEMP[3].xxxx").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: END").unwrap();
    shader
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::MAX_GPU_CLIP_MASKS;

    #[test]
    fn fragment_shader_contains_every_protocol_clip_mask() {
        let shader = fragment_shader(MAX_GPU_CLIP_MASKS);
        let rounded_mode_constant = MAX_GPU_CLIP_MASKS * CONSTANTS_PER_CLIP_MASK + 1;
        let rounded_last_constant = rounded_mode_constant + 4;
        assert!(shader.contains(&format!("CONST[0][0..{rounded_last_constant}]")));
        assert_eq!(
            shader.matches(": IF TEMP[1].xxxx").count(),
            MAX_GPU_CLIP_MASKS * 4
        );
        assert_eq!(
            shader.matches("SLT TEMP[1].y, IN[1].xxxx").count(),
            MAX_GPU_CLIP_MASKS
        );
        assert_eq!(
            shader
                .matches(&format!("CONST[0][{rounded_mode_constant}].yyyy"))
                .count(),
            MAX_GPU_CLIP_MASKS
        );
        let flat = fragment_shader(0);
        assert!(flat.contains("DCL CONST[0][0..5]"));
        assert!(!flat.contains(&format!("CONST[0][{rounded_mode_constant}]")));
    }

    #[test]
    fn gaussian_shader_uses_normalized_separable_kernel() {
        let [near, middle, far, center] = GAUSSIAN_WEIGHTS;
        assert!((center + 2.0 * (near + middle + far) - 1.0).abs() < 1.0e-6);
        let shader = fragment_shader(0);
        assert_eq!(shader.matches("TEMP[4], IMM[19]").count(), 7);
        assert!(!shader.contains("TEMP[4], IMM[0].wwww, TEMP[0]"));
    }
}
