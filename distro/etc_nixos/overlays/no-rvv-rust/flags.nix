# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: disable default vector code generation on the K1 target.
# Embedded zve subsets can be enabled independently of v. Explicit
# #[target_feature] specializations still require the final ELF scan.

"-C target-feature=-v,-zve32x,-zve32f,-zve64x,-zve64f,-zve64d"
