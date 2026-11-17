//! FontAwesome 7.2 Free: `<i class="fa fa-car">` (what merman emits for `fa:fa-car`) becomes a
//! glyph span the resvg foreignObject fallback paints with the FA fonts from FONT_DIR.

use std::collections::HashMap;

/// name -> (codepoint, styles), styles ⊆ "brs" (brands, regular, solid). Includes aliases.
pub(crate) struct FaIcons(HashMap<&'static str, (char, &'static str)>);

impl FaIcons {
    pub(crate) fn load() -> Self {
        let table = include_str!("fa_icons.txt");
        let map = table
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut it = l.split(' ');
                let (name, hex, styles) = (it.next()?, it.next()?, it.next()?);
                Some((
                    name,
                    (char::from_u32(u32::from_str_radix(hex, 16).ok()?)?, styles),
                ))
            })
            .collect();
        Self(map)
    }

    /// The replacement markup for an `<i>` with this class list, or None if it is not an icon.
    /// The prefix picks the font like FA's CSS does (`fab` brands, `far` regular 400, else
    /// solid 900); a glyph missing from that font stays missing, as in Chrome.
    pub(crate) fn glyph_span(&self, class: &str) -> Option<String> {
        let mut family = ("Font Awesome 7 Free", 900);
        let mut glyph = None;
        for token in class.split_ascii_whitespace() {
            match token {
                "fab" | "fa-brands" => family = ("Font Awesome 7 Brands", 400),
                "far" | "fa-regular" => family = ("Font Awesome 7 Free", 400),
                _ => {
                    if let Some(name) = token.strip_prefix("fa-") {
                        glyph = glyph.or(self.0.get(name).map(|g| g.0));
                    }
                }
            }
        }
        let glyph = glyph?;
        Some(format!(
            r#"<span style="font-family:'{}';font-weight:{}">&#x{:x};</span>"#,
            family.0, family.1, glyph as u32
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_classes_to_glyphs() {
        let fa = FaIcons::load();
        assert_eq!(
            fa.glyph_span("fa fa-car").as_deref(),
            Some(
                r#"<span style="font-family:'Font Awesome 7 Free';font-weight:900">&#xf1b9;</span>"#
            )
        );
        assert_eq!(
            fa.glyph_span("fa fa-automobile"),
            fa.glyph_span("fa fa-car")
        );
        assert!(
            fa.glyph_span("fab fa-github")
                .unwrap()
                .contains("Brands';font-weight:400\">&#xf09b;")
        );
        assert!(
            fa.glyph_span("far fa-user")
                .unwrap()
                .contains("Free';font-weight:400")
        );
        assert_eq!(fa.glyph_span("fa fa-no-such-icon"), None);
        assert_eq!(fa.glyph_span("italic"), None);
    }
}
