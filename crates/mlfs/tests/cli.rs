//! CLI 的端到端测试：真的把二进制跑起来，只看它的输入输出与磁盘效果。
//!
//! 每个测试自带一套临时 XDG 目录，因此不会碰到真实配置，彼此也不会串。

use std::path::PathBuf;
use std::process::{Command, Output};

/// 一次运行的隔离环境。
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// 在工作区里造一个文件，返回它的路径。
    fn write(&self, rel: &str, content: &[u8]) -> PathBuf {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// 默认的 fs 根目录（由 XDG_DATA_HOME 推导）。
    fn root(&self) -> PathBuf {
        self.path("xdg-data/mlfs")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mlfs"));
        cmd.env("XDG_DATA_HOME", self.path("xdg-data"));
        cmd.env("XDG_CONFIG_HOME", self.path("xdg-config"));
        cmd.env("HOME", self.dir.path());
        cmd.env_remove("MLFS_ROOT");
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.args(args);
        cmd.output().unwrap()
    }

    /// 跑一次并断言成功，返回 stdout。
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "mlfs {args:?} 本该成功却失败了\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// 跑一次并断言失败，返回 stderr。
    fn fail(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "mlfs {args:?} 本该失败却成功了");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// 数据目录里块文件的个数。
    fn blob_count(&self) -> usize {
        fn walk(dir: &std::path::Path, n: &mut usize) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, n);
                } else {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(&self.root().join("data"), &mut n);
        n
    }
}

#[test]
fn import_then_cat_roundtrips() {
    let env = Env::new();
    let src = env.write("hello.txt", b"hello there\n");
    let id = src.to_str().unwrap();

    env.ok(&["import", id]);
    let listed = env.ok(&["ls"]);
    assert!(listed.contains(id), "ls 里应当有刚导入的 id：{listed}");

    assert_eq!(env.ok(&["cat", id]), "hello there\n");
}

#[test]
fn ids_default_to_absolute_paths() {
    let env = Env::new();
    let src = env.write("sub/deep.txt", b"x\n");
    env.ok(&["import", src.to_str().unwrap()]);

    let listed = env.ok(&["ls"]);
    let listed = listed.trim();
    assert!(
        listed.starts_with('/'),
        "id 应当是从根开始写全的绝对路径：{listed}"
    );
    assert_eq!(PathBuf::from(listed), std::fs::canonicalize(&src).unwrap());
}

#[test]
fn id_flag_overrides_the_default_path_id() {
    let env = Env::new();
    let src = env.write("named.txt", b"payload\n");
    env.ok(&["import", src.to_str().unwrap(), "--id", "my/custom-id"]);

    let listed = env.ok(&["ls"]);
    assert!(listed.contains("my/custom-id"), "ls: {listed}");
    assert_eq!(env.ok(&["cat", "my/custom-id"]), "payload\n");
}

#[test]
fn importing_a_directory_needs_the_recursive_flag() {
    let env = Env::new();
    let dir = env.write("tree/a.txt", b"a\n");
    let dir = dir.parent().unwrap();

    let stderr = env.fail(&["import", dir.to_str().unwrap()]);
    assert!(stderr.contains("-R"), "报错里应当提示需要 -R：{stderr}");

    env.ok(&["import", "-R", dir.to_str().unwrap()]);
    assert!(env.ok(&["ls"]).contains("a.txt"));
}

#[test]
fn recursive_import_honours_gitignore_until_asked_not_to() {
    let env = Env::new();
    env.write("tree/kept.txt", b"keep\n");
    env.write("tree/ignored.txt", b"drop\n");
    env.write("tree/.gitignore", b"ignored.txt\n");
    // ignore 默认要求这确实是个 git 仓库才认 .gitignore
    std::fs::create_dir_all(env.path("tree/.git")).unwrap();
    let dir = env.path("tree");

    env.ok(&["import", "-R", dir.to_str().unwrap()]);
    let listed = env.ok(&["ls"]);
    assert!(listed.contains("kept.txt"), "ls: {listed}");
    assert!(
        !listed.contains("ignored.txt"),
        "被 .gitignore 排除的文件不该进来：{listed}"
    );

    env.ok(&["import", "-R", "--no-ignore", dir.to_str().unwrap()]);
    let listed = env.ok(&["ls"]);
    assert!(
        listed.contains("ignored.txt"),
        "--no-ignore 之后它就该进来了：{listed}"
    );
}

#[test]
fn recursive_import_skips_hidden_files_until_asked() {
    let env = Env::new();
    env.write("tree/.hidden.txt", b"hidden\n");
    env.write("tree/visible.txt", b"visible\n");
    let dir = env.path("tree");

    env.ok(&["import", "-R", dir.to_str().unwrap()]);
    assert!(!env.ok(&["ls"]).contains(".hidden.txt"));

    env.ok(&["import", "-R", "--hidden", dir.to_str().unwrap()]);
    assert!(env.ok(&["ls"]).contains(".hidden.txt"));
}

#[test]
fn export_writes_the_file_and_archives_what_it_overwrote() {
    let env = Env::new();
    let src = env.write("src.txt", b"FROM FS\n");
    env.ok(&["import", src.to_str().unwrap()]);

    // 目标不存在：直接写
    let dest = env.path("dest.txt");
    env.ok(&["export", src.to_str().unwrap(), dest.to_str().unwrap()]);
    assert_eq!(std::fs::read(&dest).unwrap(), b"FROM FS\n");

    // 目标被本地改过：默认先把这份改动存进 fs，再覆盖
    std::fs::write(&dest, b"LOCAL CHANGES\n").unwrap();
    env.ok(&["export", src.to_str().unwrap(), dest.to_str().unwrap()]);
    assert_eq!(std::fs::read(&dest).unwrap(), b"FROM FS\n");

    let archive_id = std::fs::canonicalize(&dest).unwrap();
    assert_eq!(
        env.ok(&["cat", archive_id.to_str().unwrap()]),
        "LOCAL CHANGES\n",
        "被覆盖掉的那份内容应当能从 fs 里取回来"
    );
}

#[test]
fn export_refuse_leaves_the_target_untouched() {
    let env = Env::new();
    let src = env.write("src.txt", b"FROM FS\n");
    env.ok(&["import", src.to_str().unwrap()]);

    let dest = env.path("dest.txt");
    std::fs::write(&dest, b"KEEP ME\n").unwrap();

    let stderr = env.fail(&[
        "export",
        src.to_str().unwrap(),
        dest.to_str().unwrap(),
        "--overwrite",
        "refuse",
    ]);
    assert!(stderr.contains("目标已存在"), "stderr: {stderr}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"KEEP ME\n");
}

#[test]
fn export_force_overwrites_without_archiving() {
    let env = Env::new();
    let src = env.write("src.txt", b"FROM FS\n");
    env.ok(&["import", src.to_str().unwrap()]);

    let dest = env.path("dest.txt");
    std::fs::write(&dest, b"GONE\n").unwrap();
    env.ok(&[
        "export",
        src.to_str().unwrap(),
        dest.to_str().unwrap(),
        "--overwrite",
        "force",
    ]);

    assert_eq!(std::fs::read(&dest).unwrap(), b"FROM FS\n");
    // force 不留档：目标路径不该作为 id 出现在 fs 里
    assert!(!env.ok(&["ls"]).contains("dest.txt"));
}

#[test]
fn history_lists_every_version_of_one_id() {
    let env = Env::new();
    let src = env.write("doc.txt", b"first revision\n");
    let id = src.to_str().unwrap();

    env.ok(&["import", id]);
    std::fs::write(&src, b"second revision\n").unwrap();
    env.ok(&["import", id]);

    let text = env.ok(&["history", id]);
    assert_eq!(
        text.lines()
            .filter(|l| l.trim_start().starts_with(char::is_numeric))
            .count(),
        2,
        "应当列出两个版本：\n{text}"
    );
    // 默认取最新版本
    assert_eq!(env.ok(&["cat", id]), "second revision\n");
    // 也能点名要最老的
    assert_eq!(env.ok(&["cat", id, "--first"]), "first revision\n");
}

#[test]
fn cat_of_an_unknown_id_says_so_in_plain_words() {
    let env = Env::new();
    let stderr = env.fail(&["cat", "/no/such/id"]);
    assert!(
        stderr.contains("没有 id 为 /no/such/id 的文件"),
        "不该把 redb 的内部错误原样抛出来：{stderr}"
    );
}

#[test]
fn mv_renames_and_keeps_the_content() {
    let env = Env::new();
    let src = env.write("old.txt", b"payload\n");
    env.ok(&["import", src.to_str().unwrap()]);

    env.ok(&["mv", src.to_str().unwrap(), "new-name"]);
    assert_eq!(env.ok(&["cat", "new-name"]), "payload\n");
    assert!(!env.ok(&["ls"]).contains(src.to_str().unwrap()));

    let stderr = env.fail(&["mv", "new-name", "new-name"]);
    assert!(stderr.contains("已经存在"), "stderr: {stderr}");
}

#[test]
fn rm_then_gc_frees_the_blobs() {
    let env = Env::new();
    let src = env.write("trash.txt", b"to be removed\n");
    env.ok(&["import", src.to_str().unwrap()]);
    assert!(env.blob_count() > 0);

    env.ok(&["rm", "-y", src.to_str().unwrap()]);
    assert!(env.ok(&["ls"]).trim().is_empty(), "删除后不该还有 id");
    assert!(env.blob_count() > 0, "删除只减引用，不该立刻动磁盘");

    let dry = env.ok(&["gc", "--dry-run"]);
    assert!(dry.contains("可回收"), "dry-run 只该报告：{dry}");
    assert!(env.blob_count() > 0, "dry-run 不能真删");

    env.ok(&["gc"]);
    assert_eq!(env.blob_count(), 0, "gc 之后块应当没了");
}

#[test]
fn rm_asks_before_deleting_when_stdin_is_not_a_terminal() {
    let env = Env::new();
    let src = env.write("guarded.txt", b"keep\n");
    env.ok(&["import", src.to_str().unwrap()]);

    // 测试里 stdin 不是终端，所以必须显式 --yes 才让删
    let stderr = env.fail(&["rm", src.to_str().unwrap()]);
    assert!(stderr.contains("--yes"), "stderr: {stderr}");
    assert!(env.ok(&["ls"]).contains("guarded.txt"), "不该删掉");
}

#[test]
fn ls_json_is_machine_readable() {
    let env = Env::new();
    let src = env.write("one.txt", b"1\n");
    env.ok(&["import", src.to_str().unwrap()]);

    let text = env.ok(&["ls", "--json"]);
    let parsed: Vec<String> = serde_json::from_str(&text).expect("ls --json 应当输出合法 JSON");
    assert_eq!(parsed.len(), 1);
    assert!(parsed[0].ends_with("one.txt"));
}

#[test]
fn history_json_carries_version_size_and_chunk_count() {
    let env = Env::new();
    let src = env.write("sized.txt", b"0123456789\n");
    env.ok(&["import", src.to_str().unwrap()]);

    let text = env.ok(&["history", src.to_str().unwrap(), "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let row = &parsed[0];
    assert_eq!(row["size"], 11);
    assert_eq!(row["chunks"], 1);
    assert!(row["version"].is_number());
}

#[test]
fn config_file_then_env_then_cli_decide_the_root() {
    let env = Env::new();
    let from_toml = env.path("from-toml");
    std::fs::create_dir_all(env.path("xdg-config/mlfs")).unwrap();
    std::fs::write(
        env.path("xdg-config/mlfs/config.toml"),
        format!("root = \"{}\"\n", from_toml.display()),
    )
    .unwrap();

    let root_in_use = |out: &Output| {
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.strip_prefix("fs 根目录：").map(str::to_string))
            .expect("应当有一行报告根目录")
    };

    // 配置文件说了算
    let out = env.run(&["-v", "ls"]);
    assert_eq!(root_in_use(&out), from_toml.display().to_string());

    // 环境变量赢过配置文件
    let from_env = env.path("from-env");
    let out = env.run_with(&["-v", "ls"], &[("MLFS_ROOT", from_env.to_str().unwrap())]);
    assert_eq!(root_in_use(&out), from_env.display().to_string());

    // 命令行赢过环境变量
    let from_cli = env.path("from-cli");
    let out = env.run_with(
        &["-v", "--root", from_cli.to_str().unwrap(), "ls"],
        &[("MLFS_ROOT", from_env.to_str().unwrap())],
    );
    assert_eq!(root_in_use(&out), from_cli.display().to_string());
}

#[test]
fn codec_can_be_set_in_config_and_overridden_per_run() {
    let env = Env::new();
    std::fs::create_dir_all(env.path("xdg-config/mlfs")).unwrap();
    std::fs::write(
        env.path("xdg-config/mlfs/config.toml"),
        format!(
            "root = \"{}\"\ncodec = \"gzip\"\nlevel = 6\n",
            env.root().display()
        ),
    )
    .unwrap();

    // 够大才压得动：小文件会按设计退回原样存储
    let gz = env.write("gz.txt", b"aaaa\n".repeat(20_000).as_slice());
    let zs = env.write("zs.txt", b"bbbb\n".repeat(20_000).as_slice());
    env.ok(&["import", gz.to_str().unwrap()]);
    env.ok(&["import", zs.to_str().unwrap(), "--codec", "zstd"]);

    let tags = first_bytes(&env.root().join("data"));
    assert!(tags.contains(&2), "配置文件指定的 gzip 应当出现：{tags:?}");
    assert!(
        tags.contains(&1),
        "--codec zstd 应当覆盖配置里的 gzip：{tags:?}"
    );
}

/// 递归收集数据目录下每个块文件的第一个字节（也就是编码标签）。
fn first_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut tags = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return tags;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            tags.extend(first_bytes(&path));
        } else if let Ok(bytes) = std::fs::read(&path)
            && let Some(&tag) = bytes.first()
        {
            tags.push(tag);
        }
    }
    tags
}

#[test]
fn missing_config_file_is_reported_instead_of_silently_ignored() {
    let env = Env::new();
    let stderr = env.fail(&["--config", "/no/such/config.toml", "ls"]);
    assert!(stderr.contains("配置文件不存在"), "stderr: {stderr}");
}

#[test]
fn empty_fs_lists_nothing_and_gc_is_a_no_op() {
    let env = Env::new();
    assert!(env.ok(&["ls"]).trim().is_empty());
    assert_eq!(env.blob_count(), 0);
    let text = env.ok(&["gc", "--dry-run"]);
    assert!(text.contains("可回收 0 个块"), "text: {text}");
}

#[test]
fn single_version_commands_reject_all_instead_of_picking_one() {
    let env = Env::new();
    let src = env.write("multi.txt", b"content\n");
    env.ok(&["import", src.to_str().unwrap()]);

    // 默默挑一个会让使用者拿到自己没要的版本却浑然不觉，所以要报错
    let stderr = env.fail(&["cat", src.to_str().unwrap(), "--all"]);
    assert!(stderr.contains("--all"), "stderr: {stderr}");
    let stderr = env.fail(&[
        "export",
        src.to_str().unwrap(),
        env.path("o.txt").to_str().unwrap(),
        "--all",
    ]);
    assert!(stderr.contains("--all"), "stderr: {stderr}");
    assert!(!env.path("o.txt").exists(), "报错时不该写出任何文件");
}

#[test]
fn empty_paths_in_the_config_mean_unset_not_empty() {
    let env = Env::new();
    std::fs::create_dir_all(env.path("xdg-config/mlfs")).unwrap();
    std::fs::write(
        env.path("xdg-config/mlfs/config.toml"),
        format!(
            "root = \"{}\"\n\n[storage]\ndb_path = \"\"\ndata_path = \"\"\ntemp_dir = \"\"\n",
            env.root().display()
        ),
    )
    .unwrap();

    let src = env.write("f.txt", b"payload\n");
    env.ok(&["import", src.to_str().unwrap()]);

    // 空字符串应当被当作「没写」，于是回落到 root 下面那套默认位置
    assert!(env.root().join("db.redb").is_file(), "数据库应在 root 下");
    assert!(env.root().join("data").is_dir(), "数据目录应在 root 下");
    assert_eq!(env.ok(&["cat", src.to_str().unwrap()]), "payload\n");
}
