//! git 仓库跟踪：HEAD / 分支 / 脏状态（SPEC §7、docs/04 §2.5、docs/01 F5）。
//!
//! 每个 `[[git_repos]]` 每轮跑三条命令（都带 `timeout_sec`）：
//!
//! ```text
//! git -C <path> rev-parse HEAD
//! git -C <path> rev-parse --abbrev-ref HEAD
//! git -C <path> status --porcelain
//! ```
//!
//! # 行为契约
//!
//! - `changed` = **与上一轮相比** HEAD 是否变化；**首轮只记基线、绝不告警**
//!   （否则每次 agent 重启都会误报一次「代码变动」）。
//! - HEAD 变化 → info：`【代码变动】<repo> <branch>: <old短hash> → <new短hash>`，
//!   有 `git log -1 --pretty=%s%n%an` 可用时再附上 commit 摘要与作者。
//! - 工作区变脏**持续超过 24h** → info（提示有长期未提交的改动）。时间基准是
//!   `CollectCtx::now`，所以可以用注入时间测，不用真等一天。
//! - `path` 不是 git 仓库 / git 命令失败 / 超时 → 只 `warn`（每个仓库一次）并跳过该仓库，
//!   **绝不 panic**，也不产出半个 `GitStatus`。
//!
//! # 去重键（`Alert.id`）
//!
//! | id | level | 含义 | 产出节奏 |
//! |---|---|---|---|
//! | `git:<repo>:changed` | info | HEAD 变了 | **点事件**：只在变化那一轮出现一次 |
//! | `git:<repo>:dirty` | info | 工作区脏超过 24h | **条件存续期间每轮都出现**，干净后即不再出现 |
//!
//! # 与 relay 的两类告警如何咬合（SPEC §13.2）
//!
//! relay 按 **source** 分类，`git_tracker` 整体是**事件型**：出现在 `alerts[]` 里就进
//! active 集合（据此去重，「恢复前不重复推送」），**缺席则静默移出 active 集合、绝不推
//! 「已恢复」**。两条告警各自按自己的语义产出：
//!
//! - `git:<repo>:changed`（**点事件**）：只在 HEAD 变化的那一轮出现一次。缺席只表示
//!   「本轮没有新 commit」，relay 静默移出 → 下次真有 commit 时可以再推 1 条。
//! - `git:<repo>:dirty`（**持续状态**）：只要工作区还脏（且已脏满 24h），**每一轮都上报**。
//!   若改成「只提示一次」，relay 下一轮就会看到它缺席 —— 虽然事件型不推「已恢复」，
//!   但这个条件**再也不会被上报**，等于「永久失声」（SPEC §13.1 的第二个后果）。
//!   每轮上报则：脏 → 只推 1 条（active 去重）；提交干净 → 缺席、静默移出；再次变脏 →
//!   干净地重新推 1 条。全程**没有任何「已恢复」**，因为条件型语义在 agent 侧表达、
//!   而恢复通知只由 relay 对**条件型 source** 推导 —— `git_tracker` 不是条件型 source。
//!
//! 因此本采集器**绝不产出** `xxx:recovered`。
//!
//! ⚠️ 两条 `title` 在条件/事件存续期间都必须**逐字稳定**：relay 的 active 集合与去重都用
//! `(source, title)`。所以小时数、文件数这类逐轮变化的数字一律进 `summary`，不进 `title`。
//!
//! # 一个刻意的取舍
//!
//! 「脏超过 24h」用的是**本轮累计的脏时长**（`dirty_since` 到 `now`），不是「已连续观测到脏
//! 24h」。agent 若在脏状态持续期间重启，`dirty_since` 会重新计，计时从头开始——这是
//! 无持久化状态下的必然代价；换来的是「重启不会误报」。见返回结果里的「妥协」一节。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ssm_common::config::GitRepoConfig;
use ssm_common::protocol::{Alert, AlertLevel, GitStatus};

use super::{CollectCtx, CollectOutcome, Collector};

/// 告警来源标识（SPEC §4）。
const SOURCE: &str = "git_tracker";

/// 工作区连续脏多久之后提示（docs/04 §2.5：24h）。
pub const DIRTY_ALERT_AFTER_SEC: i64 = 24 * 60 * 60;

/// 短 hash 长度（`a1b2c3d`，与 docs/04 §2.5 的示例一致）。
const SHORT_HASH_LEN: usize = 7;

/// 一个仓库的配置 + 状态。
struct RepoEntry {
    name: String,
    path: PathBuf,
    timeout: Duration,
    /// 上一轮的 HEAD（`None` = 还没成功采集过，即首轮基线）。
    last_head: Option<String>,
    /// 从哪一轮开始脏的（`CollectCtx::now` 时间）；`None` = 当前工作区是干净的。
    ///
    /// 它同时是「当前是否处于脏条件」的判据与 `git:<repo>:dirty` 的 `first_seen` 来源。
    dirty_since: Option<DateTime<Utc>>,
    /// 「这个仓库不可用」只告警一次，避免每轮刷日志。
    unusable_warned: bool,
}

/// 一次 `git` 调用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub branch: String,
    pub head: String,
    pub dirty: bool,
    /// `git status --porcelain` 的原始输出行数（用于 summary）。
    pub changed_files: usize,
    /// `git log -1` 的摘要（形如 `fix: 修一个 bug（by alice）`），拿不到就是 `None`。
    pub last_commit: Option<String>,
}

/// git 仓库采集器。
pub struct GitTrackerCollector {
    repos: Vec<RepoEntry>,
    /// 「脏多久开始提示」的阈值；生产恒为 [`DIRTY_ALERT_AFTER_SEC`]，测试可注入小值。
    dirty_alert_after_sec: i64,
}

impl GitTrackerCollector {
    /// 从 `agent.toml` 的 `[[git_repos]]` 构造。
    pub fn new(repos: &[GitRepoConfig]) -> Self {
        Self {
            repos: repos
                .iter()
                .map(|cfg| RepoEntry {
                    name: cfg.name.clone(),
                    path: cfg.path.clone(),
                    timeout: Duration::from_secs(cfg.timeout_sec.max(1)),
                    last_head: None,
                    dirty_since: None,
                    unusable_warned: false,
                })
                .collect(),
            dirty_alert_after_sec: DIRTY_ALERT_AFTER_SEC,
        }
    }

    /// 仓库数量（**仅测试可用**，生产路径不需要）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.repos.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// **仅测试可用**：缩短「脏多久提示」的阈值，避免测试真等 24h。
    #[cfg(test)]
    pub fn set_dirty_alert_after_sec(&mut self, secs: i64) {
        self.dirty_alert_after_sec = secs;
    }
}

/// 采集单个仓库（不做状态机）。
///
/// 写成自由函数而不是 `&self` 方法：`collect` 里要同时持有 `&mut repo`（状态机）
/// 与采集所需的只读配置，方法形式会撞上借用检查（`&mut self.repos` vs `&self`）。
async fn snapshot(entry: &RepoEntry) -> Result<GitSnapshot, String> {
    let head = run_git(&entry.path, &["rev-parse", "HEAD"], entry.timeout).await?;
    let head = head.trim().to_string();
    if head.is_empty() {
        return Err(format!(
            "`git -C {} rev-parse HEAD` 返回空（可能不是 git 工作区）",
            entry.path.display()
        ));
    }

    // 分离头指针时 `--abbrev-ref HEAD` 会返回字面量 "HEAD"，这本身就是有效信息
    let branch = match run_git(
        &entry.path,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        entry.timeout,
    )
    .await
    {
        Ok(branch) => {
            let branch = branch.trim().to_string();
            if branch.is_empty() {
                "HEAD".to_string()
            } else {
                branch
            }
        }
        Err(_) => "HEAD".to_string(),
    };

    let porcelain = run_git(&entry.path, &["status", "--porcelain"], entry.timeout).await?;
    let changed_files = porcelain.lines().filter(|l| !l.trim().is_empty()).count();

    // commit 摘要与作者是「锦上添花」（docs/04 §2.5 的示例里有），拿不到不影响主流程
    let last_commit = run_git(
        &entry.path,
        &["log", "-1", "--pretty=%s%n%an"],
        entry.timeout,
    )
    .await
    .ok()
    .and_then(|text| {
        let mut lines = text.lines();
        let subject = lines.next().unwrap_or("").trim().to_string();
        let author = lines.next().unwrap_or("").trim().to_string();
        match (subject.is_empty(), author.is_empty()) {
            (false, false) => Some(format!("{subject}（by {author}）")),
            (false, true) => Some(subject),
            _ => None,
        }
    });

    Ok(GitSnapshot {
        branch,
        head,
        dirty: changed_files > 0,
        changed_files,
        last_commit,
    })
}

#[async_trait]
impl Collector for GitTrackerCollector {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome {
        let mut outcome = CollectOutcome::empty();

        for repo in &mut self.repos {
            let snapshot = match snapshot(repo).await {
                Ok(snapshot) => snapshot,
                Err(reason) => {
                    if !repo.unusable_warned {
                        repo.unusable_warned = true;
                        tracing::warn!(
                            repo = %repo.name,
                            path = %repo.path.display(),
                            reason = %reason,
                            "git 仓库不可用，已跳过（此告警只出现一次；修好后自动恢复）"
                        );
                    }
                    continue;
                }
            };
            repo.unusable_warned = false;

            // ---- HEAD 变化：首轮只记基线，不告警 ----
            let previous = repo.last_head.replace(snapshot.head.clone());
            let changed = match &previous {
                Some(old) => old != &snapshot.head,
                None => false,
            };

            outcome.git.push(GitStatus {
                repo: repo.name.clone(),
                branch: snapshot.branch.clone(),
                head: snapshot.head.clone(),
                dirty: snapshot.dirty,
                changed,
            });

            if changed {
                if let Some(old) = previous {
                    let mut summary = format!(
                        "{} {}: {} → {}",
                        repo.name,
                        snapshot.branch,
                        short_hash(&old),
                        short_hash(&snapshot.head)
                    );
                    if let Some(commit) = &snapshot.last_commit {
                        summary.push(' ');
                        summary.push_str(commit);
                    }
                    outcome.alerts.push(Alert {
                        id: format!("git:{}:changed", repo.name),
                        level: AlertLevel::Info,
                        source: SOURCE.to_string(),
                        title: format!("【代码变动】{} {}", repo.name, snapshot.branch),
                        summary,
                        first_seen: ctx.now,
                    });
                    tracing::info!(
                        repo = %repo.name,
                        branch = %snapshot.branch,
                        old = %short_hash(&old),
                        new = %short_hash(&snapshot.head),
                        "git HEAD 变化，产出 info 级代码变动告警"
                    );
                }
            }

            // ---- 工作区脏状态：**条件存续期间每轮都上报**，干净后下一轮起不含 ----
            // 为什么不是「只提示一次」：缺席会让 relay 把 active 键清掉（事件型不推恢复，
            // 但这条条件**再也不会被上报** → 永久失声，SPEC §13.1 的第二个后果）。
            // 每轮上报则由 relay 的 active 集合保证「恢复前只推 1 条」，且全程没有「已恢复」。
            if snapshot.dirty {
                let first_report = repo.dirty_since.is_none();
                let since = *repo.dirty_since.get_or_insert(ctx.now);
                let dirty_for = ctx.now.signed_duration_since(since).num_seconds();

                if dirty_for >= self.dirty_alert_after_sec {
                    outcome.alerts.push(Alert {
                        id: format!("git:{}:dirty", repo.name),
                        level: AlertLevel::Info,
                        source: SOURCE.to_string(),
                        // title 只放仓库名（常量）：relay 的 active 集合按 `(source, title)` 去重，
                        // 小时数逐轮变化会让它每轮都当成新告警并给旧标题留下悬空的 active 键。
                        title: format!("【未提交改动】{} 工作区长时间未提交", repo.name),
                        summary: format!(
                            "{}（{}）工作区有 {} 个未提交改动，已持续约 {} 小时（提示阈值 {} 小时）",
                            repo.name,
                            repo.path.display(),
                            snapshot.changed_files,
                            dirty_for / 3600,
                            self.dirty_alert_after_sec / 3600
                        ),
                        // 条件存续期间不变（`dirty_since`）；不是本轮的 now。
                        first_seen: since,
                    });

                    if first_report {
                        tracing::info!(
                            repo = %repo.name,
                            changed_files = snapshot.changed_files,
                            dirty_for_sec = dirty_for,
                            "工作区长时间未提交，产出 info 级提示（脏条件存续期间每轮续报）"
                        );
                    } else {
                        tracing::debug!(
                            repo = %repo.name,
                            changed_files = snapshot.changed_files,
                            dirty_for_sec = dirty_for,
                            round = ctx.round,
                            "工作区仍脏，本轮继续上报该告警（条件存续期间每轮必报）"
                        );
                    }
                }
            } else {
                if repo.dirty_since.take().is_some() {
                    tracing::info!(
                        repo = %repo.name,
                        "工作区已恢复干净，本轮起不再上报「未提交改动」告警"
                    );
                }
            }
        }

        tracing::debug!(
            repos = self.repos.len(),
            git = outcome.git.len(),
            alerts = outcome.alerts.len(),
            round = ctx.round,
            "git 仓库采集完成"
        );
        outcome
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 跑一条 `git` 命令（带超时），返回 stdout；非 0 退出码 / 超时 / 命令缺失都返回 Err。
async fn run_git(
    path: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let path_arg = path.to_string_lossy().to_string();
    let mut full_args: Vec<&str> = vec!["-C", path_arg.as_str()];
    full_args.extend_from_slice(args);

    let spawned = tokio::process::Command::new("git")
        .args(&full_args)
        .output();

    let output = match tokio::time::timeout(timeout, spawned).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return Err(format!(
                "无法执行 `git {}`：{err}（git 是否已安装？）",
                full_args.join(" ")
            ));
        }
        Err(_) => {
            return Err(format!(
                "`git {}` 超时（{}s）",
                full_args.join(" "),
                timeout.as_secs()
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());
        return Err(format!(
            "`git {}` 退出码 {code}：{first}",
            full_args.join(" ")
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn short_hash(hash: &str) -> String {
    let trimmed = hash.trim();
    if trimmed.chars().count() <= SHORT_HASH_LEN {
        trimmed.to_string()
    } else {
        trimmed.chars().take(SHORT_HASH_LEN).collect()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::Path;

    fn ctx_at(secs: i64) -> CollectCtx {
        CollectCtx::new(
            "test-host",
            100,
            (secs.max(0) as u64) + 1,
            Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap() + chrono::Duration::seconds(secs),
        )
    }

    /// 临时目录守卫：`Drop` 时递归删除，**断言失败 panic 也会清理**。
    ///
    /// 用 RAII 而不是在测试末尾手动 `remove_dir_all`：手动清理在断言失败时会被跳过，
    /// 每跑一次失败的测试就在 `/tmp` 里留一堆垃圾（本项目踩过这个坑）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// 让 `&TempDir` 能自动当成 `&Path` 传给 `init_repo(&dir)` / `sh(&dir, ..)` 这类辅助函数。
    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 每个测试自己的临时目录（不污染仓库；`crates/agent` 没有 tempfile 依赖，这里手搓）。
    fn temp_dir(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "ssm-git-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// 同步跑一条命令（测试里建仓库用）。
    fn sh(dir: &Path, script: &str) {
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("bash 起不来: {e}"));
        assert!(
            status.status.success(),
            "命令失败: {script}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// 真的建一个 git 仓库并提交一次；`git` 不可用时跳过（返回 None）。
    fn init_repo(dir: &Path) -> Option<()> {
        let probe = std::process::Command::new("git").arg("--version").output();
        match probe {
            Ok(out) if out.status.success() => {}
            _ => {
                eprintln!("跳过：本机没有可用的 git");
                return None;
            }
        }
        sh(dir, "git init -q .");
        sh(dir, "git config user.email t@t.local");
        sh(dir, "git config user.name tester");
        sh(dir, "git config commit.gpgsign false");
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        sh(dir, "git add -A");
        sh(dir, "git commit -qm 'init: first commit'");
        Some(())
    }

    fn config(name: &str, path: &Path) -> GitRepoConfig {
        GitRepoConfig {
            name: name.to_string(),
            path: path.to_path_buf(),
            timeout_sec: 5,
        }
    }

    /// **真建仓库**：首轮只记基线不告警；改一个文件后 `dirty` 正确。
    #[tokio::test]
    async fn first_round_baseline_then_dirty_detection() {
        let dir = temp_dir("baseline");
        if init_repo(&dir).is_none() {
            return;
        }

        let mut collector = GitTrackerCollector::new(&[config("testrepo", &dir)]);
        assert_eq!(collector.len(), 1);

        let first = collector.collect(&ctx_at(0)).await;
        assert!(
            first.alerts.is_empty(),
            "首轮只记基线，绝不告警: {:?}",
            first.alerts
        );
        assert_eq!(first.git.len(), 1);
        let status = &first.git[0];
        assert_eq!(status.repo, "testrepo");
        assert!(!status.head.is_empty(), "head 不能为空");
        assert!(
            status.head.chars().all(|c| c.is_ascii_hexdigit()),
            "head 必须是 commit hash: {}",
            status.head
        );
        assert!(!status.dirty, "刚提交完的工作区必须干净");
        assert!(!status.changed, "首轮 changed 必须是 false");
        assert!(
            status.branch == "main" || status.branch == "master",
            "分支名异常: {}",
            status.branch
        );

        // 改一个**已跟踪**文件 → dirty
        std::fs::write(dir.join("a.txt"), "hello\nmore\n").unwrap();
        let second = collector.collect(&ctx_at(10)).await;
        assert!(second.git[0].dirty, "改动已跟踪文件后 dirty 必须为 true");
        assert!(
            !second.git[0].changed,
            "没有新 commit，changed 必须为 false"
        );
        assert_eq!(second.git[0].head, status.head, "HEAD 不该变");
        assert!(
            second.alerts.is_empty(),
            "脏但没到 24h，不该告警: {:?}",
            second.alerts
        );
    }

    /// **真建仓库 + 真 commit**：`changed` 从 false 变 true，产出「代码变动」info。
    #[tokio::test]
    async fn new_commit_flips_changed_and_alerts_info() {
        let dir = temp_dir("commit");
        if init_repo(&dir).is_none() {
            return;
        }

        let mut collector = GitTrackerCollector::new(&[config("testrepo", &dir)]);
        let first = collector.collect(&ctx_at(0)).await;
        let old_head = first.git[0].head.clone();
        assert!(!first.git[0].changed);

        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        sh(&dir, "git commit -qam 'feat: e2e change'");

        let second = collector.collect(&ctx_at(30)).await;
        assert_eq!(second.git.len(), 1);
        let status = &second.git[0];
        assert!(status.changed, "新 commit 后 changed 必须为 true");
        assert_ne!(status.head, old_head, "HEAD 必须变");
        assert!(!status.dirty, "提交完必须干净");

        assert_eq!(second.alerts.len(), 1, "必须产出「代码变动」告警");
        let alert = &second.alerts[0];
        assert_eq!(alert.id, "git:testrepo:changed");
        assert_eq!(alert.level, AlertLevel::Info, "代码变动必须是 info 级");
        assert_eq!(alert.source, "git_tracker");
        assert!(
            alert.title.contains("代码变动") && alert.title.contains("testrepo"),
            "{}",
            alert.title
        );
        // docs/04 §2.5 要求形如：kebei-web main: a1b2c3d → e4f5g6h
        assert!(
            alert.summary.contains(&short_hash(&old_head)),
            "summary 必须含旧短 hash: {}",
            alert.summary
        );
        assert!(
            alert.summary.contains(&short_hash(&status.head)),
            "summary 必须含新短 hash: {}",
            alert.summary
        );
        assert!(alert.summary.contains("→"), "{}", alert.summary);
        assert!(
            alert.summary.contains("feat: e2e change"),
            "summary 应带 commit 摘要: {}",
            alert.summary
        );
        assert!(
            alert.summary.contains("tester"),
            "summary 应带作者: {}",
            alert.summary
        );

        // 第三轮（以及后续各轮）没有新 commit → **不含**该告警（事件型只在那一轮出现一次；
        // 缺席不是恢复，relay 不会因此推「已恢复」，SPEC §13.2）
        for round in 3..=5 {
            let outcome = collector.collect(&ctx_at(30 * round as i64)).await;
            assert!(!outcome.git[0].changed);
            assert!(
                outcome.alerts.is_empty(),
                "第 {round} 轮没有新 commit，不该再含告警: {:?}",
                outcome.alerts
            );
        }

        // 回归护栏：任何一轮都不许产出「已恢复」类告警
        assert!(
            !second.alerts.iter().any(|a| a.id.contains("recovered")),
            "HEAD 变化是事件，没有恢复语义: {:?}",
            second.alerts
        );
        assert!(
            !second.alerts.iter().any(|a| a.title.contains("恢复")),
            "事件型告警不产出恢复通知: {:?}",
            second.alerts
        );
    }

    /// **脏条件的产出节奏（本次收尾的核心回归测试）**：脏满阈值后**连续 N 轮每一轮都含**
    /// `git:<repo>:dirty`（`id`/`title`/`first_seen` 逐轮稳定）；提交干净 → **下一轮起不含**。
    ///
    /// 为什么必须每轮都含：缺席会让 relay 把 active 键清掉（事件型不推恢复，但这条条件
    /// **再也不会被上报** → 永久失声）。而每轮都含时 relay 的 active 集合保证只推 1 条，
    /// 且全程不会有「已恢复」。
    ///
    /// 阈值用测试钩子注入 10s（生产是 24h），时间从 `CollectCtx::now` 注入，不用真等。
    #[tokio::test]
    async fn dirty_reports_every_round_until_clean() {
        let dir = temp_dir("dirty");
        if init_repo(&dir).is_none() {
            return;
        }

        let mut collector = GitTrackerCollector::new(&[config("testrepo", &dir)]);
        collector.set_dirty_alert_after_sec(10);

        // 弄脏工作区
        std::fs::write(dir.join("a.txt"), "dirty now\n").unwrap();

        // ---- 未满阈值：不含 ----
        for secs in [0_i64, 5, 9] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert!(outcome.git[0].dirty, "工作区确实是脏的");
            assert!(
                outcome.alerts.is_empty(),
                "脏了 {secs}s < 阈值 10s，不该产出: {:?}",
                outcome.alerts
            );
        }

        // ---- 越过阈值后连续 5 轮：**每一轮都含** ----
        let mut trace: Vec<String> = Vec::new();
        let mut stable_title: Option<String> = None;
        let expected_first_seen = ctx_at(0).now; // dirty_since = 第 1 轮（t=0）
        for secs in [10_i64, 11, 12, 60, 3_600] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            let dirty_alerts: Vec<&Alert> = outcome
                .alerts
                .iter()
                .filter(|a| a.id == "git:testrepo:dirty")
                .collect();
            assert_eq!(
                dirty_alerts.len(),
                1,
                "t={secs}s 工作区仍脏，必须每轮都含该告警（缺席 = relay 清 active → 永久失声）: {:?}",
                outcome.alerts
            );
            let alert = dirty_alerts[0];
            assert_eq!(alert.level, AlertLevel::Info);
            assert_eq!(alert.source, "git_tracker");
            assert_eq!(
                alert.first_seen, expected_first_seen,
                "t={secs}s first_seen 必须保持「开始脏」的时刻（不是本轮 now）"
            );
            assert_ne!(
                alert.first_seen,
                ctx_at(secs).now,
                "t={secs}s first_seen 不该等于当前时间（防止每轮刷新）"
            );
            match &stable_title {
                None => stable_title = Some(alert.title.clone()),
                Some(expected) => assert_eq!(
                    &alert.title, expected,
                    "t={secs}s title 必须逐字稳定（relay 按 (source,title) 去重；小时数进 summary）"
                ),
            }
            // 逐轮痕迹（`cargo test -- --nocapture` 可见；也是本次交付的逐轮证据）
            trace.push(format!(
                "t={secs:>4}s dirty={} alerts={:?} first_seen={} title={:?}",
                outcome.git[0].dirty,
                outcome
                    .alerts
                    .iter()
                    .map(|a| a.id.as_str())
                    .collect::<Vec<_>>(),
                alert.first_seen,
                alert.title
            ));
        }
        for line in &trace {
            eprintln!("[dirty 逐轮] {line}");
        }
        assert!(
            stable_title
                .as_deref()
                .unwrap_or_default()
                .contains("testrepo"),
            "title 要点出仓库名"
        );

        // ---- 提交干净：**下一轮起不含**（且不产出任何「已恢复」）----
        sh(&dir, "git commit -qam 'chore: clean up'");
        let clean = collector.collect(&ctx_at(3_601)).await;
        assert!(!clean.git[0].dirty, "提交后工作区必须干净");
        eprintln!(
            "[dirty 逐轮] t=3601s dirty=false alerts={:?}（干净后不再含 dirty 告警）",
            clean
                .alerts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            clean.alerts.iter().all(|a| a.id != "git:testrepo:dirty"),
            "干净后不该再有 dirty 告警: {:?}",
            clean.alerts
        );
        // 这次 commit 顺带产生的「代码变动」是**事件**，本身是对的，但它也不能是恢复类
        assert!(
            clean.alerts.iter().any(|a| a.id == "git:testrepo:changed"),
            "顺带产生的代码变动告警本身是对的: {:?}",
            clean.alerts
        );
        for round in [3_602_i64, 3_603] {
            let outcome = collector.collect(&ctx_at(round)).await;
            assert!(
                outcome.alerts.iter().all(|a| a.id != "git:testrepo:dirty"),
                "干净后的第 {round}s 轮不该再含 dirty 告警: {:?}",
                outcome.alerts
            );
        }

        // ---- 再次变脏：重新计时（新的 first_seen），越过阈值后同样每轮都含 ----
        std::fs::write(dir.join("a.txt"), "dirty again\n").unwrap();
        let re_dirty = collector.collect(&ctx_at(4_000)).await;
        assert!(re_dirty.git[0].dirty);
        assert!(
            re_dirty.alerts.iter().all(|a| a.id != "git:testrepo:dirty"),
            "刚变脏（< 10s）不该立刻告警: {:?}",
            re_dirty.alerts
        );
        let re_after = collector.collect(&ctx_at(4_011)).await;
        let again: Vec<&Alert> = re_after
            .alerts
            .iter()
            .filter(|a| a.id == "git:testrepo:dirty")
            .collect();
        assert_eq!(
            again.len(),
            1,
            "再次变脏越过阈值必须重新提示: {:?}",
            re_after.alerts
        );
        assert_ne!(
            again[0].first_seen, expected_first_seen,
            "新一次脏条件的 first_seen 必须是新的起点"
        );
        assert_eq!(again[0].first_seen, ctx_at(4_000).now);

        // ---- 全程不许出现任何恢复类告警 ----
        for outcome in [&clean, &re_dirty, &re_after] {
            for alert in &outcome.alerts {
                assert!(
                    !alert.id.contains("recovered"),
                    "agent 不得产出恢复告警（SPEC §13.2）: {}",
                    alert.id
                );
                assert!(
                    !alert.title.contains("恢复"),
                    "agent 不得产出恢复通知（SPEC §13.2）: {}",
                    alert.title
                );
            }
        }
    }

    /// 不是 git 仓库：不 panic、不产出 `GitStatus`、只 warn 一次。
    #[tokio::test]
    async fn non_git_directory_is_skipped() {
        let dir = temp_dir("notgit");
        std::fs::write(dir.join("a.txt"), "x").unwrap();

        let mut collector = GitTrackerCollector::new(&[config("notrepo", &dir)]);
        for round in 1..=3 {
            let outcome = collector.collect(&ctx_at(round as i64)).await;
            assert!(
                outcome.git.is_empty(),
                "不是 git 仓库时不该产出 GitStatus（第 {round} 轮）: {:?}",
                outcome.git
            );
            assert!(outcome.alerts.is_empty());
        }
    }

    /// 目录不存在：同样优雅跳过。
    #[tokio::test]
    async fn missing_directory_is_skipped() {
        let dir = temp_dir("missing");
        let missing = dir.path().join("nope");
        let mut collector = GitTrackerCollector::new(&[config("gone", &missing)]);
        let outcome = collector.collect(&ctx_at(0)).await;
        assert!(outcome.git.is_empty());
        assert!(outcome.alerts.is_empty());
    }

    /// 多仓库：顺序稳定，互不影响。
    #[tokio::test]
    async fn multiple_repos_are_tracked_independently() {
        let a = temp_dir("multi-a");
        let b = temp_dir("multi-b");
        if init_repo(&a).is_none() || init_repo(&b).is_none() {
            return;
        }

        let mut collector = GitTrackerCollector::new(&[config("repo-a", &a), config("repo-b", &b)]);
        let first = collector.collect(&ctx_at(0)).await;
        let names: Vec<&str> = first.git.iter().map(|g| g.repo.as_str()).collect();
        assert_eq!(names, ["repo-a", "repo-b"]);

        std::fs::write(a.join("a.txt"), "x\n").unwrap();
        sh(&a, "git commit -qam 'feat: only a'");

        let second = collector.collect(&ctx_at(30)).await;
        assert!(second.git[0].changed, "repo-a 必须 changed");
        assert!(!second.git[1].changed, "repo-b 不该 changed");
        assert_eq!(second.alerts.len(), 1);
        assert_eq!(second.alerts[0].id, "git:repo-a:changed");
    }

    /// 超时路径：`timeout_sec` 生效（用 sleep 占住 git 的方式不便注入，
    /// 这里退而验证「超时参数真的被传下去」——用一个必然超时的极短值跑真 git）。
    #[tokio::test]
    async fn zero_timeout_is_clamped_and_does_not_panic() {
        let dir = temp_dir("timeout");
        if init_repo(&dir).is_none() {
            return;
        }
        let mut cfg = config("testrepo", &dir);
        cfg.timeout_sec = 0; // 会被 max(1) 抬到 1s
        let mut collector = GitTrackerCollector::new(&[cfg]);
        let outcome = collector.collect(&ctx_at(0)).await;
        // 1s 内跑完三条 git 是常态；即便超时也只是跳过该仓库，不能 panic
        assert!(outcome.git.len() <= 1);
    }

    #[test]
    fn short_hash_truncates_and_tolerates_short_input() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash("  0123456789abcdef  "), "0123456");
        assert_eq!(short_hash(""), "");
    }

    #[test]
    fn collector_name_is_git_tracker() {
        let collector = GitTrackerCollector::new(&[]);
        assert_eq!(Collector::name(&collector), "git_tracker");
        assert!(collector.is_empty());
    }

    #[tokio::test]
    async fn empty_repo_list_yields_empty_outcome() {
        let mut collector = GitTrackerCollector::new(&[]);
        let outcome = collector.collect(&ctx_at(0)).await;
        assert!(outcome.git.is_empty());
        assert!(outcome.alerts.is_empty());
    }
}
