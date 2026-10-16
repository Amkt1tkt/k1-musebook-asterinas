# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: retain static raw probes alongside upstream libdrm tests.

# K1 MUSE Book --- (only the libdrm API probes require target shared libraries)
# EXTRA_C_FLAGS := -ldrm
EXTRA_C_FLAGS := -static
include ../../common/Makefile

$(addprefix $(OBJ_OUTPUT_DIR)/,get_info gem_create_dumb_buffer kms_properties kms_resources): EXTRA_C_FLAGS := -ldrm
# K1 MUSE Book ---
