// Unicode General Category lookup, generated from data/extracted/DerivedGeneralCategory.txt.
//
// `get_general_category(ch)` does a binary search on the compact static range
// table `GC_RANGES` produced by `build.rs`.  Any codepoint not listed in the
// table is `Unassigned` (`Cn`).
//
// To update the Unicode data, run:
//   sh scripts/fetch_unicode_data.sh [VERSION]

include!(concat!(env!("OUT_DIR"), "/general_category_data.rs"));

/// Unicode General Category (all 30 values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneralCategory {
    // C* — Other
    Control,      // Cc
    Format,       // Cf
    Unassigned,   // Cn
    PrivateUse,   // Co
    Surrogate,    // Cs
    // L* — Letter
    LowercaseLetter,  // Ll
    ModifierLetter,   // Lm
    OtherLetter,      // Lo
    TitlecaseLetter,  // Lt
    UppercaseLetter,  // Lu
    // M* — Mark
    SpacingMark,    // Mc
    EnclosingMark,  // Me
    NonspacingMark, // Mn
    // N* — Number
    DecimalNumber, // Nd
    LetterNumber,  // Nl
    OtherNumber,   // No
    // P* — Punctuation
    ConnectorPunctuation, // Pc
    DashPunctuation,      // Pd
    ClosePunctuation,     // Pe
    FinalPunctuation,     // Pf
    InitialPunctuation,   // Pi
    OtherPunctuation,     // Po
    OpenPunctuation,      // Ps
    // S* — Symbol
    CurrencySymbol, // Sc
    ModifierSymbol, // Sk
    MathSymbol,     // Sm
    OtherSymbol,    // So
    // Z* — Separator
    LineSeparator,       // Zl
    ParagraphSeparator,  // Zp
    SpaceSeparator,      // Zs
}

impl GeneralCategory {
    /// Map a `u8` category ID (as stored in `GC_RANGES`) to the enum variant.
    /// The ordering must match `GC_NAMES` in `build.rs`.
    const fn from_id(id: u8) -> Self {
        match id {
            GC_CC => Self::Control,
            GC_CF => Self::Format,
            GC_CN => Self::Unassigned,
            GC_CO => Self::PrivateUse,
            GC_CS => Self::Surrogate,
            GC_LL => Self::LowercaseLetter,
            GC_LM => Self::ModifierLetter,
            GC_LO => Self::OtherLetter,
            GC_LT => Self::TitlecaseLetter,
            GC_LU => Self::UppercaseLetter,
            GC_MC => Self::SpacingMark,
            GC_ME => Self::EnclosingMark,
            GC_MN => Self::NonspacingMark,
            GC_ND => Self::DecimalNumber,
            GC_NL => Self::LetterNumber,
            GC_NO => Self::OtherNumber,
            GC_PC => Self::ConnectorPunctuation,
            GC_PD => Self::DashPunctuation,
            GC_PE => Self::ClosePunctuation,
            GC_PF => Self::FinalPunctuation,
            GC_PI => Self::InitialPunctuation,
            GC_PO => Self::OtherPunctuation,
            GC_PS => Self::OpenPunctuation,
            GC_SC => Self::CurrencySymbol,
            GC_SK => Self::ModifierSymbol,
            GC_SM => Self::MathSymbol,
            GC_SO => Self::OtherSymbol,
            GC_ZL => Self::LineSeparator,
            GC_ZP => Self::ParagraphSeparator,
            GC_ZS => Self::SpaceSeparator,
            _ => Self::Unassigned,
        }
    }
}

/// Return the Unicode General Category of `ch`.
///
/// Uses binary search on `GC_RANGES`.  Codepoints not listed are `Unassigned`.
pub fn get_general_category(ch: char) -> GeneralCategory {
    let cp = ch as u32;
    match GC_RANGES.binary_search_by(|&(lo, hi, _)| {
        if cp < lo {
            std::cmp::Ordering::Greater
        } else if cp > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(idx) => GeneralCategory::from_id(GC_RANGES[idx].2),
        Err(_) => GeneralCategory::Unassigned,
    }
}
