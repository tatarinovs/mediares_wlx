//! Total Commander Plugin API definitions.

use std::os::raw::{c_char, c_int};

// Field types for WDX
pub const FT_NOMOREFIELDS: c_int = 0;
pub const FT_NUMERIC_32: c_int = 1;
pub const FT_NUMERIC_64: c_int = 2;
pub const FT_NUMERIC_FLOATING: c_int = 3;
pub const FT_DATE: c_int = 4;
pub const FT_TIME: c_int = 5;
pub const FT_BOOLEAN: c_int = 6;
pub const FT_MULTIPLECHOICE: c_int = 7;
pub const FT_STRING: c_int = 8;
pub const FT_FULLTEXT: c_int = 9;
pub const FT_DATETIME: c_int = 10;
pub const FT_STRINGW: c_int = 11;
pub const FT_NUMERIC_32_UN: c_int = 12;
pub const FT_NUMERIC_64_UN: c_int = 13;

// Return values for ContentGetValue / ContentGetValueW
pub const FT_FILEERROR: c_int = -1;
pub const FT_FIELDEMPTY: c_int = -2;
pub const FT_ONEDAYS: c_int = -3;
pub const FT_DELAYED: c_int = 0;

// Flags for ContentGetValue / ContentGetValueW
pub const CONTENT_DELAYIFSLOW: c_int = 1;
pub const CONTENT_PASSTHROUGH: c_int = 2;

/// `ContentDefaultParamStruct` / `ListDefaultParamStruct` — both have the same layout.
#[repr(C)]
pub struct DefaultParamStruct {
    pub size: c_int,
    pub plugin_interface_version_low: u32,
    pub plugin_interface_version_hi: u32,
    pub default_ini_name: [c_char; 260],
}

pub type ContentDefaultParamStruct = DefaultParamStruct;
pub type ListDefaultParamStruct = DefaultParamStruct;

// WLX Return codes for ListLoadNext / ListLoadNextW
pub const LISTPLUGIN_OK: c_int = 0;
pub const LISTPLUGIN_ERROR: c_int = 1;
