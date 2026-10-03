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
pub const FT_FULLTEXTW: c_int = 12;

// Return values for ContentGetValue / ContentGetValueW (contplug.h)
/// Invalid field index.
pub const FT_NOSUCHFIELD: c_int = -1;
/// File I/O error.
pub const FT_FILEERROR: c_int = -2;
/// The field exists but has no value for this file.
pub const FT_FIELDEMPTY: c_int = -3;
pub const FT_ONDEMAND: c_int = -4;
pub const FT_NOTSUPPORTED: c_int = -5;
pub const FT_SETCANCEL: c_int = -6;
/// Takes long: TC asks again from its background thread.
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

// WLX ListSendCommand commands
pub const LC_COPY: c_int = 1;
pub const LC_NEWPARAMS: c_int = 2;
pub const LC_SELECTALL: c_int = 3;
pub const LC_SETPERCENT: c_int = 4;

// WLX show flags (ListLoad / ListLoadNext / LC_NEWPARAMS parameter)
pub const LCP_WRAPTEXT: c_int = 1;
pub const LCP_FITTOWINDOW: c_int = 2;
pub const LCP_ANSI: c_int = 4;
pub const LCP_ASCII: c_int = 8;
pub const LCP_FORCESHOW: c_int = 16;
pub const LCP_FITLARGERONLY: c_int = 32;
pub const LCP_CENTER: c_int = 64;
/// TC 11+: TC uses its dark theme (also re-sent via `LC_NEWPARAMS` when it switches).
pub const LCP_DARKMODE: c_int = 128;

#[cfg(test)]
mod tests {
    use super::*;

    /// The values TC's SDK (`contplug.h`) defines; TC reads the raw numbers.
    #[test]
    fn content_return_codes_match_the_sdk() {
        assert_eq!(
            [
                FT_NOSUCHFIELD,
                FT_FILEERROR,
                FT_FIELDEMPTY,
                FT_ONDEMAND,
                FT_NOTSUPPORTED,
                FT_SETCANCEL,
                FT_DELAYED,
            ],
            [-1, -2, -3, -4, -5, -6, 0]
        );
        assert_eq!((FT_STRINGW, FT_FULLTEXTW), (11, 12));
    }
}
