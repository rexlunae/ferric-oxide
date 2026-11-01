//! Shared x86-64 vector spill instructions for standard, realtime and exception entry.

macro_rules! vector_memory {
    ($prefix:literal, $base:literal, $suffix:literal) => {
        concat!($prefix, $base, $suffix)
    };
}
pub(super) use vector_memory;

macro_rules! save_ymm {
    ($base:literal) => {
        concat!(
            vector_memory!("vextractf128 [", $base, " + 240], ymm0, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 224], ymm1, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 208], ymm2, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 192], ymm3, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 176], ymm4, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 160], ymm5, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 144], ymm6, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 128], ymm7, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 112], ymm8, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 96], ymm9, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 80], ymm10, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 64], ymm11, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 48], ymm12, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 32], ymm13, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 16], ymm14, 1\n"),
            vector_memory!("vextractf128 [", $base, " + 0], ymm15, 1\n"),
        )
    };
}
pub(super) use save_ymm;

macro_rules! restore_ymm {
    ($base:literal) => {
        concat!(
            vector_memory!("vinsertf128 ymm0, ymm0, [", $base, " + 240], 1\n"),
            vector_memory!("vinsertf128 ymm1, ymm1, [", $base, " + 224], 1\n"),
            vector_memory!("vinsertf128 ymm2, ymm2, [", $base, " + 208], 1\n"),
            vector_memory!("vinsertf128 ymm3, ymm3, [", $base, " + 192], 1\n"),
            vector_memory!("vinsertf128 ymm4, ymm4, [", $base, " + 176], 1\n"),
            vector_memory!("vinsertf128 ymm5, ymm5, [", $base, " + 160], 1\n"),
            vector_memory!("vinsertf128 ymm6, ymm6, [", $base, " + 144], 1\n"),
            vector_memory!("vinsertf128 ymm7, ymm7, [", $base, " + 128], 1\n"),
            vector_memory!("vinsertf128 ymm8, ymm8, [", $base, " + 112], 1\n"),
            vector_memory!("vinsertf128 ymm9, ymm9, [", $base, " + 96], 1\n"),
            vector_memory!("vinsertf128 ymm10, ymm10, [", $base, " + 80], 1\n"),
            vector_memory!("vinsertf128 ymm11, ymm11, [", $base, " + 64], 1\n"),
            vector_memory!("vinsertf128 ymm12, ymm12, [", $base, " + 48], 1\n"),
            vector_memory!("vinsertf128 ymm13, ymm13, [", $base, " + 32], 1\n"),
            vector_memory!("vinsertf128 ymm14, ymm14, [", $base, " + 16], 1\n"),
            vector_memory!("vinsertf128 ymm15, ymm15, [", $base, " + 0], 1\n"),
        )
    };
}
pub(super) use restore_ymm;

macro_rules! save_avx512 {
    ($base:literal) => {
        concat!(
            "test byte ptr [rip + {supports_avx512}], {avx512_enabled}\n",
            "jz 92f\n",
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 0 * 32], zmm0, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 1 * 32], zmm1, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 2 * 32], zmm2, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 3 * 32], zmm3, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 4 * 32], zmm4, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 5 * 32], zmm5, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 6 * 32], zmm6, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 7 * 32], zmm7, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 8 * 32], zmm8, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 9 * 32], zmm9, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 10 * 32], zmm10, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 11 * 32], zmm11, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 12 * 32], zmm12, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 13 * 32], zmm13, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 14 * 32], zmm14, 1\n"
            ),
            vector_memory!(
                "vextractf64x4 [",
                $base,
                " + {avx512_hi256} + 15 * 32], zmm15, 1\n"
            ),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 0 * 64], zmm16\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 1 * 64], zmm17\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 2 * 64], zmm18\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 3 * 64], zmm19\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 4 * 64], zmm20\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 5 * 64], zmm21\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 6 * 64], zmm22\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 7 * 64], zmm23\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 8 * 64], zmm24\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 9 * 64], zmm25\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 10 * 64], zmm26\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 11 * 64], zmm27\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 12 * 64], zmm28\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 13 * 64], zmm29\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 14 * 64], zmm30\n"),
            vector_memory!("vmovdqu64 [", $base, " + {avx512_hi16} + 15 * 64], zmm31\n"),
            "test byte ptr [rip + {supports_avx512}], {avx512_wide_masks}\n",
            "jnz 91f\n",
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 0 * 8], k0\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 1 * 8], k1\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 2 * 8], k2\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 3 * 8], k3\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 4 * 8], k4\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 5 * 8], k5\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 6 * 8], k6\n"),
            vector_memory!("kmovw [", $base, " + {avx512_masks} + 7 * 8], k7\n"),
            "jmp 92f\n",
            "91:\n",
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 0 * 8], k0\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 1 * 8], k1\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 2 * 8], k2\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 3 * 8], k3\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 4 * 8], k4\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 5 * 8], k5\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 6 * 8], k6\n"),
            vector_memory!("kmovq [", $base, " + {avx512_masks} + 7 * 8], k7\n"),
            "92:\n",
        )
    };
}
pub(super) use save_avx512;

macro_rules! restore_avx512 {
    ($base:literal) => {
        concat!(
            "test byte ptr [rip + {supports_avx512}], {avx512_enabled}\n",
            "jz 92f\n",
            vector_memory!(
                "vinsertf64x4 zmm0, zmm0, [",
                $base,
                " + {avx512_hi256} + 0 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm1, zmm1, [",
                $base,
                " + {avx512_hi256} + 1 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm2, zmm2, [",
                $base,
                " + {avx512_hi256} + 2 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm3, zmm3, [",
                $base,
                " + {avx512_hi256} + 3 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm4, zmm4, [",
                $base,
                " + {avx512_hi256} + 4 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm5, zmm5, [",
                $base,
                " + {avx512_hi256} + 5 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm6, zmm6, [",
                $base,
                " + {avx512_hi256} + 6 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm7, zmm7, [",
                $base,
                " + {avx512_hi256} + 7 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm8, zmm8, [",
                $base,
                " + {avx512_hi256} + 8 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm9, zmm9, [",
                $base,
                " + {avx512_hi256} + 9 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm10, zmm10, [",
                $base,
                " + {avx512_hi256} + 10 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm11, zmm11, [",
                $base,
                " + {avx512_hi256} + 11 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm12, zmm12, [",
                $base,
                " + {avx512_hi256} + 12 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm13, zmm13, [",
                $base,
                " + {avx512_hi256} + 13 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm14, zmm14, [",
                $base,
                " + {avx512_hi256} + 14 * 32], 1\n"
            ),
            vector_memory!(
                "vinsertf64x4 zmm15, zmm15, [",
                $base,
                " + {avx512_hi256} + 15 * 32], 1\n"
            ),
            vector_memory!("vmovdqu64 zmm16, [", $base, " + {avx512_hi16} + 0 * 64]\n"),
            vector_memory!("vmovdqu64 zmm17, [", $base, " + {avx512_hi16} + 1 * 64]\n"),
            vector_memory!("vmovdqu64 zmm18, [", $base, " + {avx512_hi16} + 2 * 64]\n"),
            vector_memory!("vmovdqu64 zmm19, [", $base, " + {avx512_hi16} + 3 * 64]\n"),
            vector_memory!("vmovdqu64 zmm20, [", $base, " + {avx512_hi16} + 4 * 64]\n"),
            vector_memory!("vmovdqu64 zmm21, [", $base, " + {avx512_hi16} + 5 * 64]\n"),
            vector_memory!("vmovdqu64 zmm22, [", $base, " + {avx512_hi16} + 6 * 64]\n"),
            vector_memory!("vmovdqu64 zmm23, [", $base, " + {avx512_hi16} + 7 * 64]\n"),
            vector_memory!("vmovdqu64 zmm24, [", $base, " + {avx512_hi16} + 8 * 64]\n"),
            vector_memory!("vmovdqu64 zmm25, [", $base, " + {avx512_hi16} + 9 * 64]\n"),
            vector_memory!("vmovdqu64 zmm26, [", $base, " + {avx512_hi16} + 10 * 64]\n"),
            vector_memory!("vmovdqu64 zmm27, [", $base, " + {avx512_hi16} + 11 * 64]\n"),
            vector_memory!("vmovdqu64 zmm28, [", $base, " + {avx512_hi16} + 12 * 64]\n"),
            vector_memory!("vmovdqu64 zmm29, [", $base, " + {avx512_hi16} + 13 * 64]\n"),
            vector_memory!("vmovdqu64 zmm30, [", $base, " + {avx512_hi16} + 14 * 64]\n"),
            vector_memory!("vmovdqu64 zmm31, [", $base, " + {avx512_hi16} + 15 * 64]\n"),
            "test byte ptr [rip + {supports_avx512}], {avx512_wide_masks}\n",
            "jnz 91f\n",
            vector_memory!("kmovw k0, [", $base, " + {avx512_masks} + 0 * 8]\n"),
            vector_memory!("kmovw k1, [", $base, " + {avx512_masks} + 1 * 8]\n"),
            vector_memory!("kmovw k2, [", $base, " + {avx512_masks} + 2 * 8]\n"),
            vector_memory!("kmovw k3, [", $base, " + {avx512_masks} + 3 * 8]\n"),
            vector_memory!("kmovw k4, [", $base, " + {avx512_masks} + 4 * 8]\n"),
            vector_memory!("kmovw k5, [", $base, " + {avx512_masks} + 5 * 8]\n"),
            vector_memory!("kmovw k6, [", $base, " + {avx512_masks} + 6 * 8]\n"),
            vector_memory!("kmovw k7, [", $base, " + {avx512_masks} + 7 * 8]\n"),
            "jmp 92f\n",
            "91:\n",
            vector_memory!("kmovq k0, [", $base, " + {avx512_masks} + 0 * 8]\n"),
            vector_memory!("kmovq k1, [", $base, " + {avx512_masks} + 1 * 8]\n"),
            vector_memory!("kmovq k2, [", $base, " + {avx512_masks} + 2 * 8]\n"),
            vector_memory!("kmovq k3, [", $base, " + {avx512_masks} + 3 * 8]\n"),
            vector_memory!("kmovq k4, [", $base, " + {avx512_masks} + 4 * 8]\n"),
            vector_memory!("kmovq k5, [", $base, " + {avx512_masks} + 5 * 8]\n"),
            vector_memory!("kmovq k6, [", $base, " + {avx512_masks} + 6 * 8]\n"),
            vector_memory!("kmovq k7, [", $base, " + {avx512_masks} + 7 * 8]\n"),
            "92:\n",
        )
    };
}
pub(super) use restore_avx512;

// RDI already points to the public context. Keep that layout fixed while the
// handler's call stack grows below a private AVX-512 spill area.
macro_rules! call_with_avx512 {
    ($handler:literal) => {
        concat!(
            "test byte ptr [rip + {supports_avx512}], {avx512_enabled}\n",
            "jz 93f\n",
            "sub rsp, {avx512_bytes}\n",
            save_avx512!("rsp"),
            "93:\ncall ",
            $handler,
            "\n",
            "test byte ptr [rip + {supports_avx512}], {avx512_enabled}\n",
            "jz 94f\n",
            "add rsp, {avx512_bytes}\n",
            "94:\n",
        )
    };
}
pub(super) use call_with_avx512;
