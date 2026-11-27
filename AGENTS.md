# AGENTS.md

tinge 是给终端输出上色的单个 Rust 程序（远端 `iskerwin/tinge`，经 `iskerwin/homebrew-tap` 安装）。dotfiles 的 `con`（tio）和 zsh-remote 的 `ssh-to` / `telnet-to` 调用它。README.md 是给人看的说明，本文件写给 agent：改动前 MUST 先读。

本文的 MUST、MUST NOT、SHOULD、SHOULD NOT、MAY 按 RFC 2119 / RFC 8174 解释，仅在全大写时有此含义。

## 结构

| 位置 | 内容 |
|---|---|
| `src/main.rs` | 选项解析、usage 文本、读取规则、SIGUSR1 重载、`-r` |
| `src/pty.rs` | 在伪终端里运行程序，转发输入、窗口大小和信号 |
| `src/rules.rs`、`build.rs` | 内置规则集（`build.rs` 把 `rules/*.yml` 全部嵌入）、用户目录、按名字或路径找规则集 |
| `src/select.rs` | 不加 `-c` 时选规则集：`target_of` 从命令行取程序和主机，`select_set` 按各套的 `match` 选；`glob` 同 Go 的 `path.Match` |
| `src/highlight/` | 规则加载（`load.rs`）、正则（`pattern.rs`，PCRE2 字节模式 = Python bytes 语义）、颜色（`color.rs`）和插入算法（`mod.rs`） |
| `src/stream.rs` | 把输出切成片段，决定等待还是立即上色 |
| `rules/*.yml` | 内置规则集；文件名就是规则集名，新增文件即新增一套 |
| `testdata/` | `corpus.py` 生成的语料 `corpus.bin`，和每套规则的快照 `expected-<set>-<256\|rgb>.bin` |
| `tests/` | `cli.rs`（伪终端和管道里跑二进制）、`snapshot.rs`（快照）；`benches/corpus.rs` 测速度 |

## 规则集

- 每套规则 MUST 有 `match` 段，说明它用在哪：`commands`（程序名）、`hosts`（ssh / telnet 连接的主机）、`tags`（`--tags`），都是不区分大小写的通配符。内置规则里 MUST 只有一套写 `default: true`（现在是 `server`）。
- 哪类主机用哪套规则 MUST 只写在规则集的 `match` 里。调用方（zsh-remote、dotfiles）MUST NOT 自己判断，只传 `--tags` 或直接运行程序；新增一类设备 MUST 改 `match` 或新建规则集，MUST NOT 改调用方代码。
- 规则集按名字排序，第一个 `match` 命中的生效。新规则集的 `match` 和已有的重叠时，MUST 确认排序结果是想要的，并在 `src/select.rs` 的 `select` 测试里加用例。
- 正则是 Python `re` 的 bytes 写法（`\d \w \s \b` 只认 ASCII，支持定长后行断言和命名组），由 PCRE2 以字节模式、不开 UTF/UCP 执行；唯一的改写是 `\Z` → `\z`。颜色 SHOULD 用文件里 `palette` 的名字。`exclusive: true` 的规则先匹配，后面的规则不会在它的范围里上色，也会挡住紧挨在后面的匹配。
- 规则改动 MUST 用真实输出验证：在语料里加有代表性的行，看颜色落在哪些片段上（README「Test」里的手工命令）。

## 测试

- 改完 MUST 通过 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`（CI 在 macOS 和 Linux 上跑同样的命令）。
- 改了规则集或高亮算法：MUST 运行 `UPDATE=1 cargo test --test snapshot`，再用 `git diff --stat testdata` 和快照测试的逐行差异逐条确认变化都是有意的，快照 MUST 和代码在同一个 commit 里提交。MUST NOT 为了让测试通过而更新快照。
- 新增规则：MUST 先在 `testdata/corpus.py` 加能触发它的行，`uv run --no-project python testdata/corpus.py` 重新生成语料，再更新快照。
- 修 bug MUST 配一个修复前会失败的回归测试，并确认它在旧代码上确实失败。
- 伪终端和时序问题（输出丢失、退出码、Ctrl-C）MUST 在 CPU 满载时重复跑来确认（README「Test」里的循环，200 次）；MUST NOT 用加延时或放宽断言掩盖时序问题。测试里要等程序就绪再输入时，让程序先打印 `ready`（见 `tests/cli.rs` 的 `in_pty`），不要固定等待。
- 测试 MUST NOT 读写真实的 `~/.config/tinge`：单元测试把目录作为参数传入（`rules::tempdir()`），MUST NOT 改进程的环境变量（测试是多线程并行的）；集成测试给子进程设 `HOME=<临时目录>` 和 `XDG_CONFIG_HOME=""`。

## 文档

同一个 commit 里 MUST 同步更新：

- 选项：`src/main.rs` 的 `USAGE` 文本和 README 的选项表；
- 规则集、`match` 或选择逻辑：README「Rules」、规则文件开头的注释；
- 测试方法：README「Test」和本文件「测试」；
- 不兼容的变化（选项、配置路径、默认规则集、正则语义）：commit 说明里写明，发版后告诉用户；依赖它的 zsh-remote 要提高版本门槛（见下）。

改了什么要一起看哪些文件，写在 `.impact`（格式见 `impact-check -h`，`impact-check` 由 dotfiles 安装）。结束前和提交时会自动列出漏掉的项（Stop hook、`.githooks/commit-msg`；clone 后执行一次 `git config core.hooksPath .githooks`），MUST 逐项更新，确实不需要时说明原因，提交时写 `Docs-checked: <原因>`。发现漏掉的关联 MUST 补进 `.impact` 并跑 `impact-check --lint`。

## 发布

1. 改 `Cargo.toml` 的 `version`（`tinge -v` 打印它），`cargo build` 更新 `Cargo.lock`，一起提交。0.x 阶段，不兼容的变化升第二位，其余升第三位。
2. CI 通过后打 tag：`git tag -a vX.Y.Z -m "tinge X.Y.Z" && git push origin vX.Y.Z`。
3. 算 sha256 MUST 走代理：`zsh -ic 'net-with-proxy curl -sL https://github.com/iskerwin/tinge/archive/refs/tags/vX.Y.Z.tar.gz' </dev/null | shasum -a 256`。
4. 在 `~/Developer/homebrew-tap` 改 `Formula/tinge.rb` 的 `url` 和 `sha256`（描述变了也改 README 的表格），`brew style` 通过后提交推送。formula 用 `depends_on "rust" => :build` 和 `system "cargo", "install", *std_cargo_args`。
5. 本机升级 MUST 走代理：`zsh -ic 'brew-update; net-with-proxy brew upgrade iskerwin/tap/tinge' </dev/null`，再 `brew test iskerwin/tap/tinge`。
6. zsh-remote 用到了 tinge 的新选项时，MUST 提高 zsh-remote `util.zsh` 里 `_rmt_tinge` 的最低版本，并改它 README「Requirements」里的版本号。

## 提交

- MUST 直接提交到 `main`；提交说明用英文，第一行写改了什么，正文写原因和不兼容的地方。
- 提交前 MUST 跑过上面的测试；MUST NOT 提交 `target/`（`.gitignore` 已排除）。`Cargo.lock` MUST 提交。
- 加依赖用 `cargo add`，下载 MUST 走代理：`zsh -ic 'net-with-proxy cargo add <crate>' </dev/null`。
