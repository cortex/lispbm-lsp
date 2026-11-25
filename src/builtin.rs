use std::fmt::Display;

use serde::{Deserialize, Serialize};

pub static LBMB_REF: &str = include_str!("../builtin/lbmref.toml");
pub static ARRY_REF: &str = include_str!("../builtin/arrayref.toml");
pub static CRYPT_REF: &str = include_str!("../builtin/cryptref.toml");
pub static DISPLAY_REF: &str = include_str!("../builtin/displayref.toml");
pub static DSP_REF: &str = include_str!("../builtin/dspref.toml");
pub static DYN_REF: &str = include_str!("../builtin/dynref.toml");
pub static LBM_IMG_FORMAT_REF: &str = include_str!("../builtin/lbm-image-format.toml");
pub static MATH_REF: &str = include_str!("../builtin/mathref.toml");
pub static MUTEX_REF: &str = include_str!("../builtin/mutexref.toml");
pub static RAND_REF: &str = include_str!("../builtin/randomref.toml");
pub static RUNTIME_REF: &str = include_str!("../builtin/runtimeref.toml");
pub static SET_REF: &str = include_str!("../builtin/setref.toml");
pub static STRING_REF: &str = include_str!("../builtin/stringref.toml");
pub static TTF_REF: &str = include_str!("../builtin/ttfref.toml");
pub static VESC_REF: &str = include_str!("../builtin/vescref.toml");
pub static VESC_WIFI_REF: &str = include_str!("../builtin/vesc-wifiref.toml");
pub static VESC_BLE_REF: &str = include_str!("../builtin/vesc-bleref.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    Lbm,
    Array,
    Crypt,
    Display,
    Dsp,
    Dyn,
    LbmImageFormat,
    Math,
    Mutex,
    Random,
    Runtime,
    Set,
    String,
    Ttf,
    Vesc,
    VescWifi,
    VescBle,
}

impl Display for Builtin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Builtin::Lbm => "lbm.builtin.ext.toml",
            Builtin::Array => "array.builtin.ext.toml",
            Builtin::Crypt => "crypt.builtin.ext.toml",
            Builtin::Display => "display.builtin.ext.toml",
            Builtin::Dsp => "dsp.builtin.ext.toml",
            Builtin::Dyn => "dyn.builtin.ext.toml",
            Builtin::LbmImageFormat => "lbm-image-format.builtin.ext.toml",
            Builtin::Math => "math.builtin.ext.toml",
            Builtin::Mutex => "mutex.builtin.ext.toml",
            Builtin::Random => "random.builtin.ext.toml",
            Builtin::Runtime => "runtime.builtin.ext.toml",
            Builtin::Set => "set.builtin.ext.toml",
            Builtin::String => "string.builtin.ext.toml",
            Builtin::Ttf => "ttf.builtin.ext.toml",
            Builtin::Vesc => "vesc.builtin.ext.toml",
            Builtin::VescWifi => "vesc-wifi.builtin.ext.toml",
            Builtin::VescBle => "vesc-ble.builtin.ext.toml",
        };
        write!(f, "{}", name)
    }
}

impl Builtin {
    pub fn get_ref(&self) -> &'static str {
        match self {
            Builtin::Lbm => LBMB_REF,
            Builtin::Array => ARRY_REF,
            Builtin::Crypt => CRYPT_REF,
            Builtin::Display => DISPLAY_REF,
            Builtin::Dsp => DSP_REF,
            Builtin::Dyn => DYN_REF,
            Builtin::LbmImageFormat => LBM_IMG_FORMAT_REF,
            Builtin::Math => MATH_REF,
            Builtin::Mutex => MUTEX_REF,
            Builtin::Random => RAND_REF,
            Builtin::Runtime => RUNTIME_REF,
            Builtin::Set => SET_REF,
            Builtin::String => STRING_REF,
            Builtin::Ttf => TTF_REF,
            Builtin::Vesc => VESC_REF,
            Builtin::VescWifi => VESC_WIFI_REF,
            Builtin::VescBle => VESC_BLE_REF,
        }
    }
}
