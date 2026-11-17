//! 1:1 port of legacy/test/kitchensink.test.js: same requests, same assertions.
//! `sharp(...).metadata().format` becomes `image::guess_format` (and a roxmltree parse for svg);
//! pdf-lib becomes lopdf. Needs ./fonts (tools/fonts/fetch.sh) or FONT_DIR.

mod common;

use std::sync::OnceLock;

use axum::Router;
use axum::http::Method;
use common::{Resp, get, send};
use image::ImageFormat;

const FA_VERSION: &str = "7.2.0";
const KB: usize = 1024;
/// Legacy's minimum sizes for the K01 PNG (18/19 KB) and PDF (16 KB) measured Chrome's encoders.
/// resvg's PNG (~14.5 KB) and krilla's subset-font PDF (~15 KB) are smaller for the same picture;
/// these floors still prove a real render (see tests/fixtures/divergences.toml).
const PNG_MIN_KB: usize = 12;
const PDF_MIN_KB: usize = 12;
/// pdf-lib PageSizes.A4 / A3.
const A4: [f32; 2] = [595.28, 841.89];
const A3: [f32; 2] = [841.89, 1190.55];

const FLOWCHART: &str = "eyJjb2RlIjoiZ3JhcGggVERcbkFbQ2hyaXN0bWFzXSAtLT58R2V0IG1vbmV5fCBCKEdvIHNob3BwaW5nKVxuQiAtLT4gQ3tMZXQgbWUgdGhpbmt9XG5DIC0tPnxPbmV8IERbTGFwdG9wXVxuQyAtLT58VHdvfCBFW2lQaG9uZV1cbkMgLS0-fFRocmVlfCBGW2ZhOmZhLWNhciBDYXJdXG4iLCJtZXJtYWlkIjp7InRoZW1lIjoiZGVmYXVsdCJ9fQ";
const FLOWCHART_BASE64: &str = "base64:eyJjb2RlIjoiZ3JhcGggVERcbkFbQ2hyaXN0bWFzXSAtLT58R2V0IG1vbmV5fCBCKEdvIHNob3BwaW5nKVxuQiAtLT4gQ3tMZXQgbWUgdGhpbmt9XG5DIC0tPnxPbmV8IERbTGFwdG9wXVxuQyAtLT58VHdvfCBFW2lQaG9uZV1cbkMgLS0-fFRocmVlfCBGW2ZhOmZhLWNhciBDYXJdXG4iLCJtZXJtYWlkIjp7InRoZW1lIjoiZGVmYXVsdCJ9fQ";
const FLOWCHART_PAKO: &str = "pako:eNpVkM1qw0AMhF9F6NRC_AI-BBo7zSWQQHLz5iC8SnZJ9gd5TQm2373r_ECrk9B8MwwasA2ascSLUDRwrJWHPF9NZcR2yVF3gqJYjhtO4ILn-wirj02AzoQYrb98PvnVDEE1bGeMIRnrr9NTqh7-necR6mZLMYV4-qscf8II68buTY7_rxjh7PpuzlSeqWhJoCJ5IbhAx-LI6lx-mG8Kk2HHCsu8apKrQuWnzPVRU-K1tikI5qRbxwukPoXD3bdYJun5DdWW8iPci5p-AUT3W9o";
const SEQUENCE: &str = "eyJjb2RlIjoic2VxdWVuY2VEaWFncmFtXG4gICAgQWxpY2UgLT4-IEJvYjogSGVsbG8gQm9iLCBob3cgYXJlIHlvdT9cbiAgICBCb2ItLT4-Sm9objogSG93IGFib3V0IHlvdSBKb2huP1xuICAgIEJvYi0teCBBbGljZTogSSBhbSBnb29kIHRoYW5rcyFcbiAgICBCb2IteCBKb2huOiBJIGFtIGdvb2QgdGhhbmtzIVxuICAgIE5vdGUgcmlnaHQgb2YgSm9objogQm9iIHRoaW5rcyBhIGxvbmc8YnIvPmxvbmcgdGltZSwgc28gbG9uZzxici8-dGhhdCB0aGUgdGV4dCBkb2VzPGJyLz5ub3QgZml0IG9uIGEgcm93LlxuXG4gICAgQm9iLS0-QWxpY2U6IENoZWNraW5nIHdpdGggSm9obi4uLlxuICAgIEFsaWNlLT5Kb2huOiBZZXMuLi4gSm9obiwgaG93IGFyZSB5b3U_IiwibWVybWFpZCI6eyJ0aGVtZSI6ImRlZmF1bHQifX0";
const CLASS: &str = "eyJjb2RlIjoiY2xhc3NEaWFncmFtXG5cdEFuaW1hbCA8fC0tIER1Y2tcblx0QW5pbWFsIDx8LS0gRmlzaFxuXHRBbmltYWwgPHwtLSBaZWJyYVxuXHRBbmltYWwgOiAraW50IGFnZVxuXHRBbmltYWwgOiArU3RyaW5nIGdlbmRlclxuXHRBbmltYWw6ICtpc01hbW1hbCgpXG5cdEFuaW1hbDogK21hdGUoKVxuXHRjbGFzcyBEdWNre1xuXHRcdCtTdHJpbmcgYmVha0NvbG9yXG5cdFx0K3N3aW0oKVxuXHRcdCtxdWFjaygpXG5cdH1cblx0Y2xhc3MgRmlzaHtcblx0XHQtaW50IHNpemVJbkZlZXRcblx0XHQtY2FuRWF0KClcblx0fVxuXHRjbGFzcyBaZWJyYXtcblx0XHQrYm9vbCBpc193aWxkXG5cdFx0K3J1bigpXG5cdH1cblx0XHRcdFx0XHQiLCJtZXJtYWlkIjp7InRoZW1lIjoiZGVmYXVsdCJ9LCJ1cGRhdGVFZGl0b3IiOnRydWV9";
const STATE: &str = "eyJjb2RlIjoic3RhdGVEaWFncmFtXG5cdFsqXSAtLT4gU3RpbGxcblx0U3RpbGwgLS0-IFsqXVxuXG5cdFN0aWxsIC0tPiBNb3Zpbmdcblx0TW92aW5nIC0tPiBTdGlsbFxuXHRNb3ZpbmcgLS0-IENyYXNoXG5cdENyYXNoIC0tPiBbKl1cblx0XHRcdFx0XHQiLCJtZXJtYWlkIjp7InRoZW1lIjoiZGVmYXVsdCJ9LCJ1cGRhdGVFZGl0b3IiOnRydWV9";
const GANTT: &str = "eyJjb2RlIjoiZ2FudHRcblx0dGl0bGUgQSBHYW50dCBEaWFncmFtXG5cdGRhdGVGb3JtYXQgIFlZWVktTU0tRERcblx0c2VjdGlvbiBTZWN0aW9uXG5cdEEgdGFzayAgICAgICAgICAgOmExLCAyMDE0LTAxLTAxLCAzMGRcblx0QW5vdGhlciB0YXNrICAgICA6YWZ0ZXIgYTEgICwgMjBkXG5cdHNlY3Rpb24gQW5vdGhlclxuXHRUYXNrIGluIHNlYyAgICAgIDoyMDE0LTAxLTEyICAsIDEyZFxuXHRhbm90aGVyIHRhc2sgICAgICA6IDI0ZFxuXHRcdFx0XHRcdCIsIm1lcm1haWQiOnsidGhlbWUiOiJkZWZhdWx0In0sInVwZGF0ZUVkaXRvciI6dHJ1ZX0";
const PIE: &str = "eyJjb2RlIjoicGllIHRpdGxlIFBldHMgYWRvcHRlZCBieSB2b2x1bnRlZXJzXG5cdFwiRG9nc1wiIDogMzg2XG5cdFwiQ2F0c1wiIDogODVcblx0XCJSYXRzXCIgOiAxNVxuXHRcdFx0XHRcdCIsIm1lcm1haWQiOnsidGhlbWUiOiJkZWZhdWx0In0sInVwZGF0ZUVkaXRvciI6dHJ1ZX0";
const ER: &str = "eyJjb2RlIjoiZXJEaWFncmFtXG4gICAgICAgICAgQ1VTVE9NRVIgfXwuLnx7IERFTElWRVJZLUFERFJFU1MgOiBoYXNcbiAgICAgICAgICBDVVNUT01FUiB8fC0tb3sgT1JERVIgOiBwbGFjZXNcbiAgICAgICAgICBDVVNUT01FUiB8fC0tb3sgSU5WT0lDRSA6IFwibGlhYmxlIGZvclwiXG4gICAgICAgICAgQ1VTVE9NRVIge1xuICAgICAgICAgICAgc3RyaW5nIG5hbWVcbiAgICAgICAgICAgIHN0cmluZyBjdXN0TnVtYmVyXG4gICAgICAgICAgICBzdHJpbmcgc2VjdG9yXG4gICAgICAgICAgfVxuICAgICAgICAgIERFTElWRVJZLUFERFJFU1MgfHwtLW97IE9SREVSIDogcmVjZWl2ZXNcbiAgICAgICAgICBJTlZPSUNFIHx8LS18eyBPUkRFUiA6IGNvdmVyc1xuICAgICAgICAgIE9SREVSIHx8LS18eyBPUkRFUi1JVEVNIDogaW5jbHVkZXNcbiAgICAgICAgICBQUk9EVUNULUNBVEVHT1JZIHx8LS18eyBQUk9EVUNUIDogY29udGFpbnNcbiAgICAgICAgICBQUk9EVUNUIHx8LS1veyBPUkRFUi1JVEVNIDogXCJvcmRlcmVkIGluXCJcbiAgICAgICAgICAgICIsIm1lcm1haWQiOnsidGhlbWUiOiJmb3Jlc3QifSwidXBkYXRlRWRpdG9yIjpmYWxzZX0";
const ZENUML: &str = "pako:eNptk1FvmzAQx7-K57wkUkRoCOCgrlqTgTRpUyJlTxUP9eCSIBkbGbtbEvHd52BoSlRe7Lv73d3_jH3BmcgBR_gMXJcs5ch8qlAM0EbmINEO5FuRgQ18e86UkGjNCuAKjZIkXsVBF1oJzXMqTzZvLbiSgjFTYeSGi_C5x-L1HD0-rpLk6cmSXQM0ir1kHScWO0ihK7TSdcGhrnvkYoOmyk9a_skp2mqZHWkNA5FXmWctIdE8U4Xg6Ad_EyY6gJqUd4J2ikoFcmyHmljvbIZet5vdbzQTV431q3XfjeZUolbjip6YoPnkJu_jXE4mgSr4hEKorY2-Ig5_bc47dWOK_dhiXwynGRsUQKiicuhA94fSCdhubJ3JEB6eTcd2zi7Bgh_TmntX02-b99O9LniKD7LIcaSkhikuQZb0auJWcYrVEUpIcWS2OeypZirFKW9MWkX5ixBln2luw-GIoz1ltbF0lRuR3wt6kPSGAG__jeYKR57XlsDRBf_DUeg7i7nvPwRk-eB6_nKKT8ZJnNBbhn5A3Lm79InXTPG5bek6AQk83w_nhAQuWZCwbxnnhbn-fUeqldideNbb0EZ_2efUvqrmP6bPCIA";
const LOREM: &str = "eyJjb2RlIjoiZ3JhcGggVERcbiAgQVtMb3JlbSBpcHN1bSBkb2xvciBzaXQgYW1ldCw8YnIgLz5jb25zZWN0ZXR1ciBhZGlwaXNjaW5nIGVsaXQuXSIsIm1lcm1haWQiOnsidGhlbWUiOiJkZWZhdWx0In19";
const MULTI_LANG: &str = "pako:eNpdU0tv00AQ_iurPYHUVEkdp40PSG3SIkERSOREksM23sQWsR1t1kBJckjViofIqQ1CnEA4TgTFpUVF7qHt30huI6UICcp_YG3HvHwYz3yP2Vl7t4UrlkqxgmuMNDRUyJdMFD7LxZzG9CY3SLOMEolr7euUI8My6WYbrVxZsxht8quxeCVQoFxrPdBQxDXdvN-JyVxov23SNsoX10mDW43yv1zhodVGq0X9jib6_89pjArnWrFKlCpJVAhDOcJizQ3SICZtUgVNu7vTrd1p98O0-3baPbjwX37rv4hkOTFOqLk4ffN1cPo3dVPsg5gKuuw__bHT_9l_dbntX77eQ-L1_fN2pFlmZEOvKGiyMz4cfxyPxkMUhMn2-CSIk51IVtCIriDw34M_BP8wTPbAPwb_OfhHAXiyBf4A_G6kz9MHxCQ1wgKXMwLHA8eHQQ8cBwZPBOTCoBsS5-AcgHOGZl-gbtlq1GKFmjVSD_zuCFwPXB-GPXD74J6B-0mg--Ceg_slRPdn_qpep7OJiaHXhdk7Bu8deH046AXR80Qi0GfgHf2pI_MG4VoZz4nToqtY4cymc9igzCBBiVtB3xLmGjVoCSsiVWmV2HVewiWzI2ziZ92zLCN2MsuuaVj0rTdFZTdUwmleJ-IoGr9RRk2VspxlmxwrKSnsgZUWfoSVBUmel5ey6XQ6lcwuyQuC3BSaTHpekqVkOpnNZpJSRpI6c_hxuGpyXk5nUguLUiorSXJWWpTjVVdVnVssnovY3Lq7aVbimobsreiihPel8wvgplYB";
const SVG_FLOWCHART_PAKO: &str = "pako:eNpFj8GKwkAMhl8l5OSCfYEehLVVL4IL663jIXSiM8hkhnTKIm3ffccV2Vvy_V9CMmEfLWONN6Xk4Nwa-ewap37IgYYLVNVmPnCGEIUfM2xXhwiDiyl5uX0Y2T4FaKbjU2HIzst9MdL8zZ2EZ2i7I6Uc0-VNzz9xhl3nv1xZ-U-dcrH33ZXqK1U9KTSkJcY1BtZA3pYjJyMABrPjwAbrUlrSu0EjS_HGZCnzzvocFeusI6-Rxhy_H9K_-5fTeir_hhdcfgF5pVhE";
const LOREM_DARK: &str = "eyJjb2RlIjoiZ3JhcGggVERcbiAgQVtMb3JlbSBpcHN1bSBkb2xvciBzaXQgYW1ldCw8YnIgLz5jb25zZWN0ZXR1ciBhZGlwaXNjaW5nIGVsaXQuXSIsIm1lcm1haWQiOnsidGhlbWUiOiJkYXJrIn19";
const FA_PLAIN: &str = "Z3JhcGggVEQKICAgIEFbQ2hyaXN0bWFzXSAtLT58R2V0IG1vbmV5fCBCKEdvIHNob3BwaW5nKQogICAgQiAtLT4gQ3tMZXQgbWUgdGhpbmt9CiAgICBDIC0tPnxPbmV8IERbTGFwdG9wXQogICAgQyAtLT58VHdvfCBFW2lQaG9uZV0KICAgIEMgLS0+fFRocmVlfCBGW2ZhOmZhLWNhciBDYXJd";
const CLICKABLE: &str = "pako:eNpVU-9r2zAQ_VcOwUIGTTL2MawtzY-1HSUbbcYYdRmKdY61yJLRSUtD0v99JzlZV3-ST_fuvXvP3ovSKRRjURm3LWvpAyxnhQV-rh7vnFTaruH7_R1UUhtUQ_iBUEoLwe8gOKj0OnoEFwNs693wCQaDi8MM00z48vB1cYBJ_5tBSYyqsdxAqPnkLDmDCU-IuZR6QVoF6L3zoDAwHQ3fd0omaSxM97f02lw677EMly9dyzQz_0Q6wOzEODWaGWv0mepeaq6xdE0UEbSFtQ51XA0_rfzo4taWJuZlE8PKuw1aMNpuUmMqdajcC1ttDFCLqCC2-bbSzyexnZKFO8B8P9MKyDXo7BFKyDvuXGSQpjz_tMD8dYHPj1eUnWo6ixiSwR0u-de0BgNm_NP_8MR6nVlTb-na3VHxGxjHeWK9zs4efXtTKsTCwbhfiAPc9JecdqSTO0vdIFOzq3KV7LnRFJw_UlXeNUCyySZuCX2WEom94mVKjzJ0kSst1142J9vKHNYMao8VFKIOoaXxaHTMiLWPGvSN1Grwm_4djf6DA1Sa6Uc5IBpZ3F5KIr22iHTeM3KFhs5Xcd0LyPsze3r55bF1Pgwb1Qs6GDyf5MTfffyQPC0EK-i-lzy1EOJMHDn5V9knxYVIAfHVmI8KKxlNKERhX7hVxuAedrYU4-AjnonYKuaddQu_Lc6z-K728hd0LzGz";
const ELK: &str = "pako:eNqtl21vokAQx7_KZpMmXqKNIkXLi0usRWvqw0VtX_S4mBVW3BR2ybJcz6v97seDtlgXweZMBGf4_2aZ2WFdXqHFbAx1WKvVTCqIcLEO1LpJLUZXxNFNCgBYuezFWiMuUjP-rIXnDtESu4EOVsgN8PsVG69Q6IoppjbmmOvAhNh9NiEwaTKGw5G_BsOpSS8uQC-KDGyCIqcXOy7Ajcus58CkqdVllGJLEEYj1xiNF9H3pwmjI6hEh28m_BX7hYMCgzqE4kUQLvuchX6i-vDH8g8r5X48-ohiN4vsXKCy-5EKl4Q5fsjJymIOpln94RVQObRT2kaEb3zOLBwEWTbrB5WslXIe4s9YZInUAyrpOVWtMRJ-6PmLIHig5DfmAXKVLCQVgIrUnYa0GHMJdQR7wTwbKesHlayVchyvOHEwR4IdcFk_qGStlFP2sz0Ll0l3fMx1rfZ9K8-w19AWj43t6fRLxWn9pzjtM-OMjNlkNt9-muYDyWzSm99OB-N7mUrS9DFjuNEDw4lFxGab07glaWkflGSL65ATpDtRCO25Ie6j4ItZf42Sdu_xIlG6xEVgTnWLsKLCFvFHxSkCcuqSk3dMR-WOJ09a9ZNgifKUpIuqdCJM1IDHRTqtP9GwJe_3HG5X3q8OlTOf0n-pmL0ZTPqdWX6f54LdO8OYGcf3mQ9Mjc7oDH3PmM6fztC_IIG5TSSl--RIlt6Hfme63a3FUsXpNpcApZaNAi7nsZBQSbZn6HtTY3Y3Ggzvt9ICyovUGc7PkM_vBt17Y2xMz2DSNMrrz1kFCnDpkwKr0OHEhrrgIa5CD3MPxSZ8jXfAJhRr7GETxtve3U7YhCZ9i7BomX1izNuTUTBnDfVk81yFoW9Hid6mO-F3SbKH7rKQCqhfJxGg_gr_QL3dutSu1EazrjY0RW0121W4gbpSv1RaWlPTVKVRV6_V5tVbFf5NxqxftttaW1VakUBrNJpKez-mYZMov_2QKBRstqHW3sbJ1VH6jpC8Krz9A38Ap_U";
const FLOWCHART_ELK: &str =
    "pako:eJyrVkrOT0lVslJKy8kvT85ILCrRTc3JVvAJislz1NW1c4rJcwJSzhCes5KOUm5qUW5iZgpQR3WtUi0AiUQTvg";
const NEWLINES: &str = "pako:eJwljkEKwjAQRa8yZG0v4EKwiuBacZNkEZrRDE6SkkypUnp3o_7d48PjLWrIHtVW3TnPQ3BF4Ho0Cdr2-iKNLXTdDnp9w_IGzukBgi8xJklwAmeY88QemJ4IkqGOTN-TUoM4sdDI2JgpYbV_cf8zHvSJEtVg1UZFLNGRbxnLqtYPnSsyJw";
const TIDY_TREE: &str = "pako:eNo9UMtuhDAM_JXIp10JEBAeS27tbo899VblEpEAkYiDsolUivj3Ai29jDz2zNjyAq2VChjEccyxtdjpnnEkZBSzDZ4Rr-Uce6cUx0NiNEojJo7OWn-5_FGin0SQ0WJP_KCxv173jJcdXne47_DgCBH0Tktg3gUVgVHOiJ3Csgs4-EEZxYFtpVSdCKPnwHHdbJPAT2vN6XQ29AOwTozPjYVJCq8eWvROmP-uUyiVu9uAHlidHxnAFvgCltMyKW9NURRZ2tzKnEYwA8uqIqElTYu0aaqUVpSuEXwfW9OkLKosr2nepFVTZ_m59E1qb915lgjefszYnlwd0_ffFx-fXn8AwuJ0ww";
const ER_170: &str = "pako:eNp1UsFuwjAM_RUru2wSfEB7q6BIE6OgArusO3itaSPaBDnpJgb8-1JaaSsdvsRynu33bJ9EqjMSviCeSswZq0SBs0kQw_k8HusTRMEinI6n8fNrGIMPWJb6y_yiTq3bmLEsVQ5MuXQuWqlVVFcfxLCaD1AV7mkYdGTK2-jbOxyQbdfz0j6rMF4vozscpemBhhQzlp_E5kWmpAw5epCITUFQdoGHRNzmPHreE-wkGxthRQ6_VOURPA_SAhlT68oBMrXzoWxYAEpscwcfh0Irgu2fGUllAXPqKe5pHEpKkeP_Bj-C2fyO_qv8lHqgrtkiiLazYLLZxq6ZbpQ2FK2Gb2INmt2mnNZm__51k0aMREVcoczcMV3ZJcIW5NQK37kZ8r4ZycXhsLZ6fVSp8C3XNBL1IUNL3fkJf4elocsPBXTMmA";
const ER_272: &str = "pako:eNpFjrEKwzAMRH_F3FjyBd4CXTt1a9VBxEpqiO3gyEMJ_ve4SaE36R6H7jYMyQksJF89T5kDRdPUPwn9hfAy2wm-WjX7OJn-JJUiOgTJgb1rH44gQd8ShGDb6WTkMiuBYm1RLprunzjAai7SoSyOVX61sCPPa6MLx0dKfy_Oa8q3c-Uxtu7OaTo4";

fn router() -> Router {
    static APP: OnceLock<Router> = OnceLock::new();
    APP.get_or_init(|| mermaid_ink::router(common::app(&[])))
        .clone()
}

/// status 200, `resp.type`, the sniffed format and `body.length > min_kb * KB`.
async fn image(path: &str, mime: &str, format: ImageFormat, min_kb: usize) -> (u32, u32) {
    let resp = get(&router(), path).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.mime(), mime);
    assert_eq!(image::guess_format(&resp.body).unwrap(), format);
    assert!(resp.body.len() > min_kb * KB, "{} bytes", resp.body.len());
    let img = image::load_from_memory(&resp.body).unwrap();
    (img.width(), img.height())
}

async fn jpeg(path: &str, min_kb: usize) -> (u32, u32) {
    image(path, "image/jpeg", ImageFormat::Jpeg, min_kb).await
}

/// status 200, image/svg+xml, parses as an <svg> document, `body.length > min_kb * KB`.
async fn svg(path: &str, min_kb: usize) -> String {
    let resp = get(&router(), path).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.mime(), "image/svg+xml");
    let body = resp.text();
    let doc = roxmltree::Document::parse(&body).expect("svg parses");
    assert_eq!(doc.root_element().tag_name().name(), "svg");
    assert!(body.len() > min_kb * KB, "{} bytes", body.len());
    body
}

async fn status(path: &str) -> u16 {
    get(&router(), path).await.status
}

#[tokio::test]
async fn get_home() {
    let resp = get(&router(), "/").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.mime(), "text/html");
    assert_eq!(
        resp.header("content-type"),
        Some("text/html; charset=utf-8")
    );
}

// describe('/img')

#[tokio::test]
async fn img_404_without_code() {
    assert_eq!(status("/img").await, 404);
}

#[tokio::test]
async fn img_flowchart() {
    jpeg(&format!("/img/{FLOWCHART}"), 15).await;
}

#[tokio::test]
async fn img_flowchart_base64() {
    jpeg(&format!("/img/{FLOWCHART_BASE64}"), 15).await;
}

#[tokio::test]
async fn img_flowchart_pako() {
    jpeg(&format!("/img/{FLOWCHART_PAKO}"), 15).await;
}

#[tokio::test]
async fn img_flowchart_png() {
    image(
        &format!("/img/{FLOWCHART}?type=png"),
        "image/png",
        ImageFormat::Png,
        PNG_MIN_KB, // legacy: 18
    )
    .await;
}

#[tokio::test]
async fn img_flowchart_webp() {
    image(
        &format!("/img/{FLOWCHART}?type=webp"),
        "image/webp",
        ImageFormat::WebP,
        10,
    )
    .await;
}

#[tokio::test]
async fn img_flowchart_uppercase_type() {
    image(
        &format!("/img/{FLOWCHART}?type=PNG"),
        "image/png",
        ImageFormat::Png,
        PNG_MIN_KB, // legacy: 19
    )
    .await;
}

#[tokio::test]
async fn img_flowchart_unknown_type() {
    jpeg(&format!("/img/{FLOWCHART}?type=abcd"), 15).await;
}

#[tokio::test]
async fn img_sequence_diagram() {
    jpeg(&format!("/img/{SEQUENCE}"), 31).await;
}

#[tokio::test]
async fn img_class_diagram() {
    jpeg(&format!("/img/{CLASS}"), 11).await;
}

#[tokio::test]
async fn img_state_diagram() {
    jpeg(&format!("/img/{STATE}"), 5).await;
}

#[tokio::test]
async fn img_gantt_chart() {
    jpeg(&format!("/img/{GANTT}"), 15).await;
}

#[tokio::test]
async fn img_pie_chart() {
    jpeg(&format!("/img/{PIE}"), 21).await;
}

#[tokio::test]
async fn img_er_diagram() {
    jpeg(&format!("/img/{ER}"), 33).await;
}

#[tokio::test]
async fn img_zenuml_diagram() {
    jpeg(&format!("/img/{ZENUML}"), 65).await;
}

#[tokio::test]
async fn img_400_when_encoded_code_is_invalid() {
    assert_eq!(status("/img/eyJjb2RlIjoiZ3JhcGgg").await, 400);
}

#[tokio::test]
async fn img_200_when_bg_color_is_used() {
    assert_eq!(
        status(&format!("/svg/{LOREM}?bgColor=!slategray")).await,
        200
    );
}

#[tokio::test]
async fn img_200_when_theme_is_used() {
    assert_eq!(status(&format!("/svg/{LOREM}?theme=dark")).await, 200);
}

#[tokio::test]
async fn img_flowchart_default_size() {
    let (w, h) = jpeg(&format!("/img/{FLOWCHART}"), 15).await;
    // Changes to the renderer and fonts could affect these values.
    assert!(w > 300, "{w}");
    assert!(h > 430, "{h}");
}

#[tokio::test]
async fn img_flowchart_set_width() {
    let (w, h) = jpeg(&format!("/img/{FLOWCHART}?width=1000"), 15).await;
    assert_eq!(w, 1000);
    assert!(h > 1000, "{h}");
}

#[tokio::test]
async fn img_flowchart_set_height() {
    let (w, h) = jpeg(&format!("/img/{FLOWCHART}?height=1000"), 15).await;
    assert!(w > 679, "{w}");
    assert_eq!(h, 1000);
}

// This doesn't change the aspect ratio of the actual diagram, it just adds more margins.
#[tokio::test]
async fn img_flowchart_set_width_and_height() {
    let (w, h) = jpeg(&format!("/img/{FLOWCHART}?width=1000&height=1000"), 15).await;
    assert_eq!((w, h), (1000, 1000));
}

#[tokio::test]
async fn img_flowchart_set_width_with_2x_scale() {
    let (w, h) = jpeg(&format!("/img/{FLOWCHART}?width=1000&scale=2"), 15).await;
    assert_eq!(w, 2000);
    assert!(h > 2000, "{h}");
}

#[tokio::test]
async fn img_scale_without_width_or_height() {
    assert_eq!(status(&format!("/img/{FLOWCHART}?scale=2")).await, 400);
}

#[tokio::test]
async fn img_scale_below_0() {
    assert_eq!(
        status(&format!("/img/{FLOWCHART}?width=1000&scale=-1")).await,
        400
    );
}

#[tokio::test]
async fn img_scale_above_3() {
    assert_eq!(
        status(&format!("/img/{FLOWCHART}?width=1000&scale=4")).await,
        400
    );
}

#[tokio::test]
async fn img_width_below_0() {
    assert_eq!(status(&format!("/img/{FLOWCHART}?width=-1")).await, 400);
}

// `process.env.MAX_WIDTH + 1` with MAX_WIDTH unset is NaN.
#[tokio::test]
async fn img_width_above_max_width_env() {
    assert_eq!(status(&format!("/img/{FLOWCHART}?width=NaN")).await, 400);
}

#[tokio::test]
async fn img_height_above_max_height_env() {
    assert_eq!(status(&format!("/img/{FLOWCHART}?height=NaN")).await, 400);
}

#[tokio::test]
async fn img_multi_languages_and_font_awesome_icon() {
    jpeg(&format!("/img/{MULTI_LANG}"), 36).await;
}

// describe('/pdf')

fn pdf_pages(resp: &Resp) -> Vec<[f32; 2]> {
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.mime(), "application/pdf");
    let doc = lopdf::Document::load_mem(&resp.body).expect("pdf loads");
    doc.get_pages()
        .values()
        .map(|&id| {
            let b = doc.get_dictionary(id).unwrap().get(b"MediaBox").unwrap();
            let b: Vec<f32> = b
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_float().unwrap())
                .collect();
            [b[2] - b[0], b[3] - b[1]]
        })
        .collect()
}

/// "There can be a slight difference in size": within 1pt.
fn assert_size(got: [f32; 2], want: [f32; 2]) {
    assert!(
        (got[0] - want[0]).abs() < 1.0 && (got[1] - want[1]).abs() < 1.0,
        "{got:?} vs {want:?}"
    );
}

async fn pdf_first_page(query: &str) -> [f32; 2] {
    let resp = get(&router(), &format!("/pdf/{FLOWCHART}{query}")).await;
    pdf_pages(&resp)[0]
}

#[tokio::test]
async fn pdf_404_without_code() {
    assert_eq!(status("/pdf").await, 404);
}

#[tokio::test]
async fn pdf_flowchart() {
    let resp = get(&router(), &format!("/pdf/{FLOWCHART}")).await;
    assert!(
        resp.body.len() > PDF_MIN_KB * KB,
        "{} bytes",
        resp.body.len()
    ); // legacy: 16
    pdf_pages(&resp);
}

#[tokio::test]
async fn pdf_defaults_to_a4() {
    let resp = get(&router(), &format!("/pdf/{FLOWCHART}")).await;
    assert!(
        resp.body.len() > PDF_MIN_KB * KB,
        "{} bytes",
        resp.body.len()
    ); // legacy: 16
    assert_size(pdf_pages(&resp)[0], A4);
}

#[tokio::test]
async fn pdf_page_size_a3() {
    assert_size(pdf_first_page("?paper=a3").await, A3);
}

#[tokio::test]
async fn pdf_page_size_uppercase_a3() {
    assert_size(pdf_first_page("?paper=A3").await, A3);
}

#[tokio::test]
async fn pdf_unexpected_paper_defaults_to_a4() {
    assert_size(pdf_first_page("?paper=not-a-size").await, A4);
}

#[tokio::test]
async fn pdf_landscape() {
    assert_size(pdf_first_page("?landscape").await, [A4[1], A4[0]]);
}

#[tokio::test]
async fn pdf_fit_page_size_to_diagram() {
    let resp = get(
        &router(),
        &format!("/pdf/{FLOWCHART}?width=1000&height=800&fit"),
    )
    .await;
    let pages = pdf_pages(&resp);
    assert_eq!(pages.len(), 1);
    // jest toBeCloseTo(x) with the default precision 2.
    let ratio = pages[0][0] / pages[0][1];
    assert!((ratio - 1000.0 / 800.0).abs() < 0.005, "{ratio}");
}

// describe('/svg')

#[tokio::test]
async fn svg_404_without_code() {
    assert_eq!(status("/svg").await, 404);
}

#[tokio::test]
async fn svg_flowchart() {
    svg(&format!("/svg/{FLOWCHART}"), 9).await;
}

#[tokio::test]
async fn svg_flowchart_base64() {
    svg(&format!("/svg/{FLOWCHART_BASE64}"), 9).await;
}

#[tokio::test]
async fn svg_flowchart_pako() {
    svg(&format!("/svg/{SVG_FLOWCHART_PAKO}"), 9).await;
}

#[tokio::test]
async fn svg_sequence_diagram() {
    svg(&format!("/svg/{SEQUENCE}"), 8).await;
}

#[tokio::test]
async fn svg_class_diagram() {
    svg(&format!("/svg/{CLASS}"), 6).await;
}

#[tokio::test]
async fn svg_state_diagram() {
    svg(&format!("/svg/{STATE}"), 6).await;
}

#[tokio::test]
async fn svg_gantt_chart() {
    svg(&format!("/svg/{GANTT}"), 6).await;
}

#[tokio::test]
async fn svg_pie_chart() {
    svg(&format!("/svg/{PIE}"), 2).await;
}

#[tokio::test]
async fn svg_er_diagram() {
    svg(&format!("/svg/{ER}"), 12).await;
}

#[tokio::test]
async fn svg_400_when_encoded_code_is_invalid() {
    assert_eq!(status("/svg/eyJjb2RlIjoiZ3JhcGgg").await, 400);
}

async fn svg_text(path: &str) -> String {
    get(&router(), path).await.text()
}

/// `/<svg [^>]* background-color: X;/`
fn svg_open_tag_has(body: &str, needle: &str) -> bool {
    body.split_once("<svg ")
        .and_then(|(_, rest)| rest.split_once('>'))
        .is_some_and(|(tag, _)| tag.contains(needle))
}

#[tokio::test]
async fn svg_replaces_br() {
    assert!(!svg_text(&format!("/svg/{LOREM}")).await.contains("<br>"));
}

#[tokio::test]
async fn svg_background_color() {
    let body = svg_text(&format!("/svg/{LOREM}?bgColor=123123")).await;
    assert!(svg_open_tag_has(
        &body,
        " background-color: rgb(18, 49, 35);"
    ));
}

#[tokio::test]
async fn svg_named_background_color() {
    let body = svg_text(&format!("/svg/{LOREM}?bgColor=!slategray")).await;
    assert!(svg_open_tag_has(&body, " background-color: slategray;"));
}

#[tokio::test]
async fn svg_ignores_invalid_background_colors() {
    let resp = get(
        &router(),
        &format!("/svg/{LOREM}?bgColor=!non-existant-color"),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert!(!svg_open_tag_has(&resp.text(), " background-color:"));
}

// Theme style contains
// default: #mermaid-svg .node path{fill:#ECECFF;stroke:#9370DB;stroke-width:1px;}
// dark: #mermaid-svg .node path{fill:#1f2020;stroke:#ccc;stroke-width:1px;}
const DEFAULT_NODE: &str = "#mermaid-svg .node path{fill:#ECECFF;stroke:#9370DB;stroke-width:1px;}";
const DARK_NODE: &str = "#mermaid-svg .node path{fill:#1f2020;stroke:#ccc;stroke-width:1px;}";

async fn svg_200(path: &str) -> String {
    let resp = get(&router(), path).await;
    assert_eq!(resp.status, 200);
    resp.text()
}

#[tokio::test]
async fn svg_site_theme_defaults_to_default() {
    assert!(
        svg_200(&format!("/svg/{LOREM}"))
            .await
            .contains(DEFAULT_NODE)
    );
}

#[tokio::test]
async fn svg_site_theme_dark() {
    assert!(
        svg_200(&format!("/svg/{LOREM}?theme=dark"))
            .await
            .contains(DARK_NODE)
    );
}

#[tokio::test]
async fn svg_ignores_invalid_site_theme() {
    let body = svg_200(&format!("/svg/{LOREM}?theme=non-existant-theme")).await;
    assert!(body.contains(DEFAULT_NODE));
}

#[tokio::test]
async fn svg_diagram_theme_from_state() {
    assert!(
        svg_200(&format!("/svg/{LOREM_DARK}"))
            .await
            .contains(DARK_NODE)
    );
}

#[tokio::test]
async fn svg_imports_fontawesome() {
    let body = svg_text(&format!("/svg/{FA_PLAIN}")).await;
    assert!(body.contains(&format!(
        "https://cdnjs.cloudflare.com/ajax/libs/font-awesome/{FA_VERSION}/css/all.min.css"
    )));
}

#[tokio::test]
async fn svg_imports_custom_fontawesome_url() {
    // Legacy read the env per request; here it is a boot-time setting.
    let router = mermaid_ink::router(common::app(&[(
        "FONT_AWESOME_CSS_URL",
        "https://example.com/ajax/libs/font-awesome/FA_VERSION/css/all.min.css",
    )]));
    let body = get(&router, &format!("/svg/{FA_PLAIN}")).await.text();
    assert!(body.contains(&format!(
        "https://example.com/ajax/libs/font-awesome/{FA_VERSION}/css/all.min.css"
    )));
}

#[tokio::test]
async fn svg_clickable_node() {
    let body = svg_text(&format!("/svg/{CLICKABLE}")).await;
    assert!(
        body.contains(r#"<svg id="mermaid-svg" width="100%" xmlns="http://www.w3.org/2000/svg" "#),
        "{}",
        &body[..body.len().min(300)]
    );
}

#[tokio::test]
async fn svg_elk_layout() {
    let body = svg(&format!("/svg/{ELK}"), 0).await;
    // Mermaid 12 ignores defaultRenderer and detects graph/flowchart as flowchart-v2.
    assert!(body.contains(r#"aria-roledescription="flowchart-v2""#));
    assert!(body.contains(r#"class="flowchart""#));
    assert!(body.contains(r#"data-et="edge""#));
    assert!(body.contains("<p>PVpanel (PVpanel)</p>"));
}

#[tokio::test]
async fn svg_flowchart_elk_keyword() {
    let body = svg(&format!("/svg/{FLOWCHART_ELK}"), 0).await;
    assert!(body.contains(r#"aria-roledescription="flowchart-elk""#));
}

#[tokio::test]
async fn svg_newlines_in_node_labels() {
    let body = svg(&format!("/svg/{NEWLINES}"), 0).await;
    assert!(body.contains(
        "<p>Very long text<br />that I would like to split<br />into multiple<br />lines</p>"
    ));
}

#[tokio::test]
async fn svg_tidy_tree_layout() {
    let body = svg(&format!("/svg/{TIDY_TREE}"), 0).await;
    assert!(body.contains(r#"aria-roledescription="mindmap""#));
    assert!(body.contains(r#"class="mindmapDiagram""#));
    assert!(body.contains(r#"data-et="edge""#));
    assert!(body.contains("<p>mindmap is a long thing</p>"));
}

#[tokio::test]
async fn svg_multi_languages_and_font_awesome_icon() {
    svg(&format!("/svg/{MULTI_LANG}"), 14).await;
}

// describe('Fixed #170, ER diagram, works at mermaid@10')

#[tokio::test]
async fn fixed_170_img() {
    jpeg(&format!("/img/{ER_170}"), 15).await;
}

#[tokio::test]
async fn fixed_170_svg() {
    svg(&format!("/svg/{ER_170}"), 13).await;
}

// describe('Fixed #272, ER diagram')

#[tokio::test]
async fn fixed_272_img() {
    jpeg(&format!("/img/{ER_272}"), 2).await;
}

#[tokio::test]
async fn fixed_272_svg() {
    svg(&format!("/svg/{ER_272}"), 4).await;
}

// describe('CORS')

const CROSS_ORIGIN: &str = "http://cross.origin.com";

async fn preflight(path: &str) {
    let resp = send(
        &router(),
        Method::OPTIONS,
        path,
        &[
            ("Origin", CROSS_ORIGIN),
            ("Access-Control-Request-Method", "GET"),
        ],
    )
    .await;
    assert_eq!(resp.status, 204);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some(CROSS_ORIGIN)
    );
    assert_eq!(resp.header("access-control-allow-methods"), Some("GET"));
}

#[tokio::test]
async fn cors_img() {
    preflight(&format!("/img/{FLOWCHART}")).await;
}

#[tokio::test]
async fn cors_svg() {
    preflight(&format!("/svg/{FLOWCHART}")).await;
}
