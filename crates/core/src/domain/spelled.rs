//! [`spelled_enum!`]: fixed lower-case spellings for fieldless enums.

/// Implements `as_str`, `parse` and `Display` for a fieldless enum with
/// fixed lower-case spellings (the ones the store and the API use).
macro_rules! spelled_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            /// The lower-case spelling used by the store and the API.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parses the lower-case spelling.
            ///
            /// # Errors
            ///
            /// A [`ValidationError`](crate::error::ValidationError) for any
            /// other string.
            pub fn parse(raw: &str) -> Result<Self, $crate::error::ValidationError> {
                match raw {
                    $($text => Ok(Self::$variant),)+
                    _ => Err($crate::error::ValidationError::new(
                        stringify!($name),
                        $crate::error::Problem::Pattern {
                            expected: concat!("one of: ", $($text, " "),+),
                        },
                    )),
                }
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use spelled_enum;
