// SPDX-License-Identifier: MPL-2.0

use ostd::info;

pub(crate) fn init() {
    for device in aster_input::all_devices() {
        info!("Found an input device, name: {}", device.name());
    }

    // K1 MUSE Book --- (link board components so their initialization hooks run)
    #[cfg(target_arch = "riscv64")]
    {
        #[cfg(feature = "cpufreq")]
        use k1_musebook_cpufreq as _;
        use k1_musebook_drm as _;
        use k1_musebook_gpio as _;
        use k1_musebook_i2c as _;
        use k1_musebook_pinctrl as _;
        use k1_musebook_power as _;
        #[cfg(feature = "pvr")]
        use k1_musebook_pvr as _;
        use k1_musebook_thermal as _;
        use k1_musebook_usb as _;
    }
    // K1 MUSE Book ---
}
