//! Assembly alternatives and CET entry markers, accessed through explicit paths.

macro_rules! expand_bool(
    ($value:expr_2021) => {
        concat!($value)
    }
);

macro_rules! alternative(
    (feature: $feature:literal, then: [$($then:expr_2021),*], default: [$($default:expr_2021),*]) => {
        $crate::arch::macros::alternative2!(feature1: $feature, then1: [$($then),*], feature2: "", then2: [""], default: [$($default),*])
    }
);
macro_rules! saturating_sub(
    ($lhs:literal, $rhs:literal) => { concat!(
        "((", $lhs, ")>(", $rhs, "))*((", $lhs, ")-(", $rhs, "))",
    ) }
);
// Use feature1 if present, otherwise try using feature2, otherwise use default.
//
// cpu_feature_always simply means it is always enabled. Thus, if feature2, which has lower
// priority, is "always" but feature1 is "auto", feature2 will still be checked for, and feature2
// will become the fallback code.
//
// An empty string as feature is equivalent with "never".
macro_rules! alternative2(
    (feature1: $feature1:literal, then1: [$($then1:expr_2021),*], feature2: $feature2:literal, then2: [$($then2:expr_2021),*], default: [$($default:expr_2021),*]) => {
        concat!("
            .set true, 1
            .set false, 0
            40:
            .if ", $crate::arch::macros::expand_bool!(cfg!(cpu_feature_always = $feature1)), "
            ", $($then1,)* "
            .elseif ", $crate::arch::macros::expand_bool!(cfg!(cpu_feature_always = $feature2)), "
            ", $($then2,)* "
            .else
            ", $($default,)* "
            .endif
            42:
            .if ", $crate::arch::macros::expand_bool!(cfg!(cpu_feature_auto = $feature1)), "
            .skip -", $crate::arch::macros::saturating_sub!("51f - 50f", "42b - 40b"), ", 0x90
            .endif
            .if ", $crate::arch::macros::expand_bool!(cfg!(cpu_feature_auto = $feature2)), "
            .skip -", $crate::arch::macros::saturating_sub!("61f - 60f", "42b - 40b"), ", 0x90
            .endif
            41:
            ",
            // FIXME: The assembler apparently complains "invalid number of bytes" despite it being
            // quite obvious what saturating_sub does.

            // Declare them in reverse order. Last relocation wins!
            $crate::arch::macros::alternative_auto!("6", $feature2, [$($then2),*]),
            $crate::arch::macros::alternative_auto!("5", $feature1, [$($then1),*]),
        )
    };
);
macro_rules! alternative_auto(
    ($first_digit:literal, $feature:literal, [$($then:expr_2021),*]) => { concat!(
        ".if ", $crate::arch::macros::expand_bool!(cfg!(cpu_feature_auto = $feature)), "
        .pushsection .altcode.", $feature, ",\"a\"
        ", $first_digit, "0:
        ", $($then,)* "
        ", $first_digit, "1:
        .popsection
        .pushsection .altfeatures.", $feature, ",\"a\"
        70: .ascii \"", $feature, "\"
        71:
        .popsection
        .pushsection .altrelocs.", $feature, ",\"a\"
        .quad 70b
        .quad 71b - 70b
        .quad 40b
        .quad 42b - 40b
        .quad 41b - 40b
        .quad 0
        .quad ", $first_digit, "0b
        .quad ", $first_digit, "1b - ", $first_digit, "0b
        .popsection
        .endif
        ",
    ) }
);

// Ferric Oxide (IBT, see arch/x86_64/ibt.rs): `endbr64` for the start of a hand-written entry
// point that the CPU (IDT, SYSCALL) or an indirect branch reaches, when the kernel is built for
// indirect branch tracking; nothing otherwise, so the default build's code is unchanged.
#[cfg(any(cpu_feature_always = "ibt", cpu_feature_auto = "ibt"))]
macro_rules! ibt_endbr {
    () => {
        "endbr64\n"
    };
}
#[cfg(not(any(cpu_feature_always = "ibt", cpu_feature_auto = "ibt")))]
macro_rules! ibt_endbr {
    () => {
        ""
    };
}

// Path-based scope for assembly alternatives and their expansion helpers.
pub(crate) use alternative;
pub(crate) use alternative2;
pub(crate) use alternative_auto;
pub(crate) use expand_bool;
pub(crate) use ibt_endbr;
pub(crate) use saturating_sub;
