use std::fs;

fn read_readme() -> String {
    fs::read_to_string("README.md").expect("README.md must exist at workspace root")
}

fn extract_intro(content: &str) -> &str {
    // Step 1: 找到第一个 `# ` 后的内容
    let after_first_heading = content
        .split_once('#')
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");

    // Step 2: 找到第一个双换行后的连续文字 = 简介段落
    after_first_heading
        .split("\n\n")
        .find(|p| !p.trim().is_empty() && !p.trim().starts_with('#'))
        .unwrap_or("")
}

fn count_sentences(s: &str) -> usize {
    s.split(|c: char| c == '。' || c == '.')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .count()
}

/// AC-1: 简介存在且位于文件顶部，2–3 句
#[test]
fn readme_has_project_intro_at_top() {
    let content = read_readme();
    let intro = extract_intro(&content);

    assert!(
        !intro.is_empty(),
        "Expected an intro paragraph right after the first heading"
    );

    let n = count_sentences(intro);
    assert!(
        (2..=3).contains(&n),
        "Intro must be 2–3 sentences, got {}",
        n
    );
}

/// AC-2 + AC-3: 简介中提到的关键词能在 README 正文中找到对应章节
#[test]
fn readme_intro_terms_are_reflected_in_body() {
    let content = read_readme();
    let intro = extract_intro(&content);

    // 从简介中提取所有非空句子，再从中提取关键词（排除 stop-words）
    let stop_words = ["AI", "Agent", "一个", "的", "了", "和", "与", "支持", "具备", "拥有"];
    let intro_keywords: Vec<&str> = intro
        .split(|c: char| c == '。' || c == '.' || c == '，' || c == ',' || c == ' ' || c == '\n')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !stop_words.contains(s) && s.len() > 2)
        .collect();

    // 正文是第一个 ## 之后的部分（排除简介自身）
    let body_start = content.find("\n## ").unwrap_or(0);
    let body = &content[body_start..];

    for kw in &intro_keywords {
        assert!(
            body.contains(kw),
            "Keyword '{}' appears in intro but never appears in README body sections",
            kw
        );
    }
}

/// AC-4: Markdown 结构——简介后第一个非空行是 `##` 标题或 `---` 分隔符
#[test]
fn intro_paragraph_markdown_structure_is_valid() {
    let content = read_readme();
    let intro = extract_intro(&content);

    // 找到简介段落之后的内容
    let after_intro = content
        .split_once(intro)
        .map(|(_, rest)| rest.trim_start())
        .unwrap_or("");

    let first_content_line = after_intro
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");

    assert!(
        first_content_line.starts_with("## ")
            || first_content_line.starts_with("---")
            || first_content_line.starts_with("#"),
        "After intro, first non-empty line must be a section heading or separator, got: {}",
        first_content_line
    );

    // GAP-1: 简介自身不应被 Markdown 引用块包裹（任意一行以 '>' 开头），
    // 否则 GitHub 会渲染为黄色引文样式而非正式段落。
    assert!(
        !intro.lines().any(|l| l.trim_start().starts_with('>')),
        "Intro must not be inside a Markdown quote block (no line may start with '>')"
    );
}

/// AC-5: 简介句数严格在 2–3 句（≤202 字符建议值）
#[test]
fn intro_sentence_count_in_range() {
    let content = read_readme();
    let intro = extract_intro(&content);

    let n = count_sentences(intro);
    assert!(
        (2..=3).contains(&n),
        "Intro must be 2–3 sentences, got {}: '{}'",
        n,
        intro
    );

    // 建议字符数 ≤202
    assert!(
        intro.chars().count() <= 202,
        "Intro too long ({} chars), consider shortening",
        intro.chars().count()
    );
}

/// AC-6: 简介中引用项目名时使用 `latte-agent` 原文
#[test]
fn intro_uses_correct_project_name() {
    let content = read_readme();
    let intro = extract_intro(&content);

    // 如果提到项目名，必须是 `latte-agent`
    if intro.contains("latte") || intro.contains("Latte") || intro.contains("Agent") {
        assert!(
            intro.contains("latte-agent"),
            "Intro must use 'latte-agent' (lowercase, hyphenated) when referencing the project name"
        );
    }
}

/// AC-7: 简介中提到的后端模型能在 README 中找到对应配置证明
#[test]
fn intro_model_names_are_supported() {
    let content = read_readme();
    let intro = extract_intro(&content);

    let known_models = [
        "Anthropic",
        "OpenAI",
        "DeepSeek",
        "Ollama",
        "Google Gemini",
    ];

    for model in &known_models {
        if intro.contains(model) {
            assert!(
                content.contains(model),
                "Intro mentions '{}' but it never appears in README body",
                model
            );
        }
    }
}

/// AC-8: 简介不引入 markdown 格式问题——简介本身不是代码块
#[test]
fn intro_is_not_code_block() {
    let content = read_readme();
    let intro = extract_intro(&content);

    assert!(
        !intro.starts_with("```"),
        "Intro must not be a code block"
    );
    assert!(
        !intro.contains("\n```"),
        "Intro must not contain code block delimiters"
    );
}

/// GAP-3: 显式校验简介中提到的核心复合词在 README 正文中完整出现。
/// 防御通用词法分词对中文复合词边界不稳定的问题。
#[test]
fn readme_intro_compound_terms_in_body() {
    let content = read_readme();
    let intro = extract_intro(&content);

    // body = 第一个 `## ` 标题之后的部分（排除简介自身）
    let body_start = content.find("\n## ").unwrap_or(0);
    let body = &content[body_start..];

    let compound_terms = ["多角色", "自调试", "HookChain"];

    for term in &compound_terms {
        if intro.contains(term) {
            assert!(
                body.contains(term),
                "Compound term '{}' is mentioned in intro but never appears in README body",
                term
            );
        }
    }
}
