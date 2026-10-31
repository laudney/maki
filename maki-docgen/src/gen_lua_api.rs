use maki_lua::{bundled, docs_render};

use crate::lua_util::api_docs;

const FRONTMATTER: &str = r#"+++
title = "Lua API"
weight = 10
[extra]
group = "Reference"
+++

"#;

pub fn generate() -> String {
    format!(
        "{FRONTMATTER}{}",
        docs_render::site_page(bundled(), &api_docs())
    )
}
