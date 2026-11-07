//! 学习章节的「体量」：用源码本身量出来，不靠模型自评。
//!
//! 为什么需要它：以前每个知识点一律写满 10 节、没有字数上限。实测 LAT-100（Slice / Status /
//! Comparator）里证据只有 35 行代码的 `Slice` 写了 88KB，113 行的 `Status` 写了 107KB——
//! 篇幅跟模板走，跟难度毫无关系。「简单的问题少写」要有一把尺子，这就是那把尺子。
//!
//! 度量（全部从 `evidence` 指向的源码里数出来，可复算）：
//!
//! | 量 | 含义 |
//! |---|---|
//! | `lines` | 证据区间内的代码行数（不含空行和纯注释行） |
//! | `branches` | 控制流分支数：`if` `for` `while` `switch` `case` `catch` `match` `loop` `?` `&&` `\|\|` |
//! | `files` | 证据涉及的文件数 |
//! | `alternatives` | 这个知识点依据的决策里**带出处**的替代方案数 |
//! | `hard` | “短但难”的行：并发（atomic / memory_order / mutex / 条件变量…）、unsafe / 裸指针转换、宏定义 |
//!
//! `score = lines + 8 × branches + 15 × (files − 1) + 30 × alternatives + 25 × hard`
//!
//! `hard` 的权重（每行折合 25 行普通代码）补的是“代码不长但很难”：一行 `compare_exchange_weak(…, memory_order_acq_rel)` 背后的
//! 推理量比十行顺序代码大得多，只数行数和分支会把这类代码判成简单档、少写。`#define` 行原本按注释 /
//! 预处理行跳过不计入 `lines`，在这里单独计入 `hard`（宏展开要在脑子里替换一遍）。
//!
//! 分档：`< 120` 简单、`120..300` 中等、`≥ 300` 复杂。档位决定写哪几节、每节最多多少字、
//! 一个主题最多拆几个知识点。阈值用 LAT-100 的实测数据标定：35 行的 `Slice`（99 分）、
//! 25 行的 `SliceParts` 拼接（72 分）是简单档；113 行的 `Status`（200 分）、`Comparator`
//! 的两个点（151、156 分）是中等档。

use std::path::Path;

/// 简单档上限（不含）。
pub const SIMPLE_BELOW: usize = 120;
/// 复杂档下限（含）。
pub const LARGE_FROM: usize = 300;
/// 总分每满这么多分才撑得起一个知识点。
pub const SCORE_PER_POINT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Simple,
    Medium,
    Large,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Simple => "简单",
            Tier::Medium => "中等",
            Tier::Large => "复杂",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Complexity {
    pub lines: usize,
    pub branches: usize,
    pub files: usize,
    pub alternatives: usize,
    pub hard: usize,
}

impl Complexity {
    pub fn score(&self) -> usize {
        self.lines
            + 8 * self.branches
            + 15 * self.files.saturating_sub(1)
            + 30 * self.alternatives
            + 25 * self.hard
    }

    pub fn tier(&self) -> Tier {
        match self.score() {
            s if s < SIMPLE_BELOW => Tier::Simple,
            s if s < LARGE_FROM => Tier::Medium,
            _ => Tier::Large,
        }
    }

    /// 给人（和 writer）看的一句话，算式写全，可以复算。
    pub fn describe(&self) -> String {
        format!(
            "{}档（得分 {} = 代码 {} 行 + 8×分支 {} + 15×(文件 {} − 1) + 30×有出处的替代方案 {} + 25×难点行 {}）",
            self.tier().label(),
            self.score(),
            self.lines,
            self.branches,
            self.files.max(1),
            self.alternatives,
            self.hard
        )
    }
}

/// 这一档**不写**的节（`CHAPTER_PARTS` 序号）。0 代码逻辑 / 1 问题与背景 / 5 原理 / 9 总结 任何档都写。
pub fn tier_skips(tier: Tier) -> &'static [u8] {
    match tier {
        // 简单：先看问题 → 读懂代码 → 为什么这样设计 → 带走什么。其余都是同一段代码的第二遍。
        Tier::Simple => &[2, 3, 4, 6, 7, 8],
        // 中等：不单列目标、不再把同一段代码讲第二遍。
        Tier::Medium => &[2, 6],
        Tier::Large => &[],
    }
}

/// 这一档每节的字数上限（字符数，不是字节）。`None` = 这一档不写这一节。
/// 超过要求精简，重试用完按现稿放行。
pub fn part_cap(tier: Tier, part: u8) -> Option<usize> {
    // (简单, 中等, 复杂)；0 = 这一档不写
    let (s, m, l) = match part {
        0 => (4500, 8000, 12000),
        1 => (1500, 2500, 4000),
        2 => (0, 0, 3000),
        3 => (0, 3500, 5000),
        4 => (0, 3000, 4500),
        5 => (2500, 4000, 6000),
        6 => (0, 0, 6500),
        7 => (0, 2500, 4000),
        8 => (0, 2500, 3500),
        9 => (600, 800, 1200),
        _ => (0, 0, 0),
    };
    let v = match tier {
        Tier::Simple => s,
        Tier::Medium => m,
        Tier::Large => l,
    };
    (v > 0).then_some(v)
}

/// 一个主题的总分最多撑得起几个知识点（至少 1 个，最多 6 个）。
pub fn max_points(total_score: usize) -> usize {
    (total_score / SCORE_PER_POINT).clamp(1, 6)
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with('#')
}

/// “短但难”的标记（子串匹配，大小写敏感）：并发原语、unsafe / 裸指针转换。
const HARD_MARKERS: &[&str] = &[
    "atomic",
    "Atomic",
    "memory_order",
    "compare_exchange",
    "fetch_add",
    "fetch_sub",
    "mutex",
    "Mutex",
    "lock_guard",
    "unique_lock",
    "shared_lock",
    "condition_variable",
    "CondVar",
    "RwLock",
    "std::thread",
    "pthread_",
    "__sync_",
    "__atomic",
    "volatile",
    "Ordering::",
    "unsafe",
    "reinterpret_cast",
    "const_cast",
    "asm!",
    "__asm",
    "transmute",
];

/// 这一行是不是“难点行”：宏定义，或含并发 / unsafe 标记（一行最多计 1）。
fn is_hard_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#define") || HARD_MARKERS.iter().any(|m| line.contains(m))
}

/// 一行里的分支数：词边界上的关键字 + `?` `&&` `||`。
fn branches_in(line: &str) -> usize {
    let mut n = line.matches("&&").count() + line.matches("||").count() + line.matches('?').count();
    let bytes = line.as_bytes();
    for kw in [
        "if", "for", "while", "switch", "case", "catch", "match", "loop",
    ] {
        let mut from = 0;
        while let Some(pos) = line[from..].find(kw) {
            let i = from + pos;
            let j = i + kw.len();
            let before_ok =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after_ok =
                j >= bytes.len() || !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_');
            if before_ok && after_ok {
                n += 1;
            }
            from = j;
        }
    }
    n
}

/// 量一个知识点的证据。读不到的区间忽略（证据存在性由 plan 校验负责）。
pub fn measure(cwd: &Path, evidence: &str, grounded_alternatives: usize) -> Complexity {
    let mut c = Complexity {
        alternatives: grounded_alternatives,
        ..Default::default()
    };
    let mut files = std::collections::BTreeSet::new();
    for seg in crate::workflow::split_learn_evidence(evidence) {
        let Some((file, range)) = seg.rsplit_once(':') else {
            continue;
        };
        let (a, b) = range.split_once('-').unwrap_or((range, range));
        let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(cwd.join(file.trim())) else {
            continue;
        };
        files.insert(file.trim().to_string());
        for line in text
            .lines()
            .skip(a.saturating_sub(1))
            .take(b.saturating_sub(a) + 1)
        {
            let t = line.trim_start();
            // 宏定义在 is_comment_or_blank 里会被当成预处理行跳过，先单独记为难点。
            if t.starts_with("#define") {
                c.hard += 1;
                continue;
            }
            if is_comment_or_blank(line) {
                continue;
            }
            if is_hard_line(line) {
                c.hard += 1;
            }
            c.lines += 1;
            c.branches += branches_in(line);
        }
    }
    c.files = files.len();
    c
}

/// 一章锚点路径（文件或目录）的整体量，学习书目录的体量校验用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathVolume {
    /// 代码行数合计（不含空行、纯注释行）。
    pub lines: usize,
    pub files: usize,
    /// 单个文件里最大的代码行数，以及它是哪个文件。
    pub max_file_lines: usize,
    pub largest: String,
}

const SOURCE_EXTS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "rs", "go", "py", "java", "js", "ts", "tsx", "kt",
    "swift",
];

fn collect_sources(path: &Path, out: &mut Vec<std::path::PathBuf>, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    if path.is_file() {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTS.contains(&e))
        {
            out.push(path.to_path_buf());
            *budget -= 1;
        }
    } else if let Ok(rd) = std::fs::read_dir(path) {
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            collect_sources(&e, out, budget);
        }
    }
}

/// 量一组锚点路径。目录最多展开 300 个源文件；不存在的路径忽略（存在性由别处校验）。
pub fn measure_paths(cwd: &Path, paths: &[String]) -> PathVolume {
    let mut files = Vec::new();
    let mut budget = 300usize;
    for p in paths {
        collect_sources(
            &cwd.join(p.trim().trim_end_matches('/')),
            &mut files,
            &mut budget,
        );
    }
    files.sort();
    files.dedup();
    let mut v = PathVolume {
        files: files.len(),
        ..Default::default()
    };
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let n = text.lines().filter(|l| !is_comment_or_blank(l)).count();
        v.lines += n;
        if n > v.max_file_lines {
            v.max_file_lines = n;
            v.largest = f
                .strip_prefix(cwd)
                .unwrap_or(&f)
                .to_string_lossy()
                .to_string();
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_count_keywords_on_word_boundaries_and_operators() {
        assert_eq!(branches_in("if (a && b || c) {"), 3);
        assert_eq!(branches_in("for (int i = 0; i < n; ++i)"), 1);
        assert_eq!(
            branches_in("int modifier = 0;"),
            0,
            "`modifier` 里的 if 不算"
        );
        assert_eq!(branches_in("x = (r == 0) ? 1 : 2;"), 1);
        assert_eq!(branches_in("match x { _ => loop {} }"), 2);
        assert_eq!(
            branches_in("showcase(format)"),
            0,
            "case / for 在单词里面不算"
        );
    }

    #[test]
    fn tiers_follow_the_score_thresholds() {
        let c = |lines, branches, files, alternatives| Complexity {
            lines,
            branches,
            files,
            alternatives,
            hard: 0,
        };
        // LAT-100 实测：Slice 35 行 / 8 分支 / 1 文件 → 99；SliceParts 拼接 → 72；Status 113 行 → 200。
        assert_eq!(c(35, 8, 1, 0).score(), 99);
        assert_eq!(c(35, 8, 1, 0).tier(), Tier::Simple);
        assert_eq!(c(25, 4, 2, 0).score(), 72);
        assert_eq!(c(113, 9, 2, 0).score(), 200);
        assert_eq!(c(113, 9, 2, 0).tier(), Tier::Medium);
        assert_eq!(c(200, 20, 2, 2).tier(), Tier::Large);
        // 同样 35 行，带 3 个有出处的替代方案就不再是简单档：要对比的东西多了。
        assert_eq!(c(35, 8, 1, 3).tier(), Tier::Medium);
    }

    #[test]
    fn simple_tier_writes_only_the_spine_and_caps_are_per_tier() {
        assert_eq!(tier_skips(Tier::Simple), &[2, 3, 4, 6, 7, 8]);
        assert!(tier_skips(Tier::Large).is_empty());
        assert!(part_cap(Tier::Simple, 0) < part_cap(Tier::Medium, 0));
        assert!(part_cap(Tier::Medium, 0) < part_cap(Tier::Large, 0));
        assert_eq!(part_cap(Tier::Simple, 3), None, "本档不写的节没有上限");
        // 每个被 tier_skips 跳过的节，在该档都没有上限；没被跳过的都有。
        for tier in [Tier::Simple, Tier::Medium, Tier::Large] {
            for n in 0..=9u8 {
                assert_eq!(
                    tier_skips(tier).contains(&n),
                    part_cap(tier, n).is_none(),
                    "{tier:?} 第 {n} 节"
                );
            }
        }
    }

    #[test]
    fn point_count_follows_total_score() {
        // LAT-100 五个点合计 678 分：撑得起 3 个，不是 5 个。
        assert_eq!(max_points(678), 3);
        assert_eq!(max_points(50), 1);
        assert_eq!(max_points(5000), 6);
    }

    #[test]
    fn measure_counts_code_lines_skipping_comments_and_blanks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.c"),
            "// c\nint f(int x) {\n\n  if (x) return 1;\n  return 0; // t\n}\n",
        )
        .unwrap();
        let c = measure(d.path(), "a.c:1-6", 0);
        assert_eq!((c.lines, c.branches, c.files), (4, 1, 1));
        assert_eq!(measure(d.path(), "nope.c:1-6", 0).files, 0);
    }

    #[test]
    fn measure_paths_sums_files_and_dirs_and_finds_the_largest() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/sub")).unwrap();
        std::fs::write(d.path().join("src/a.cc"), "int a;\nint b;\n// c\n").unwrap();
        std::fs::write(d.path().join("src/sub/b.h"), "int x;\n").unwrap();
        std::fs::write(d.path().join("src/readme.txt"), "忽略\n").unwrap();
        let v = measure_paths(
            d.path(),
            &[
                "src".to_string(),
                "src/a.cc".to_string(),
                "nope".to_string(),
            ],
        );
        assert_eq!((v.lines, v.files, v.max_file_lines), (3, 2, 2), "{v:?}");
        assert_eq!(v.largest, "src/a.cc");
    }

    /// 短但难的代码不能被判成简单档：20 行 CAS 循环 + 宏，只数行数会是简单档。
    #[test]
    fn concurrency_unsafe_and_macros_raise_the_score() {
        let d = tempfile::tempdir().unwrap();
        let code = "#define LIKELY(x) __builtin_expect(!!(x), 1)\n#define SPIN() do { } while (0)\n\
                    bool push(Node* n) {\n  Node* h = head_.load(std::memory_order_relaxed);\n  n->next = h;\n\
                    while (!head_.compare_exchange_weak(h, n, std::memory_order_release, std::memory_order_relaxed)) {\n\
                    n->next = h;\n  }\n  auto* p = reinterpret_cast<char*>(n);\n  std::lock_guard<std::mutex> g(mu_);\n  return true;\n}\n";
        std::fs::write(d.path().join("lf.cc"), code).unwrap();
        let c = measure(d.path(), "lf.cc:1-12", 0);
        assert_eq!(
            c.hard, 6,
            "2 个宏 + load + CAS + reinterpret_cast + lock_guard: {c:?}"
        );
        assert_eq!(
            c.tier(),
            Tier::Medium,
            "10 行无锁 push 不是简单档：{}",
            c.describe()
        );
        assert!(!is_hard_line("int modifier = 0;"));
        assert!(c.describe().contains("难点行 6"));
    }
}
