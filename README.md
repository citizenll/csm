# 🌟 CSM (Codex Session Manager)
[简体中文](./README.zh-CN.md)

**The Simple, Intuitive Toolkit for Codex Sessions**

Ever run into a broken chat history, found yourself unable to continue a conversation after switching AI models, or just didn't know how to clean up your old threads? **CSM is your session manager and first-aid kit.**

It provides a highly intuitive visual interface (TUI) and simple command-line tools that let you safely inspect, repair, and migrate your Codex conversations—without ever needing to manually edit complex underlying files.

> **🚀 The Fastest Way to Start:**
> Run the binary with no arguments at all: `cargo run --`. This instantly opens the visual interactive menu!
> 
![CSM TUI Preview](assets/csm-tui-preview.en.svg)

---

## ✨ Core Highlights: What can it do for you?

1. **🏥 Rescue Broken Chats (Session Repair)**
Conversation refusing to load? CSM can help you rebuild and repair the session state directly from your underlying history logs with a single click.
2. **🚚 Painless Model/Provider Switching (Migration)**
Want to seamlessly move your current chat to a different AI model (like one with a larger context window)? CSM offers a guided "moving service" so you don't lose your place.
3. **🧳 Lightweight Successor Sessions (Distill)**
When one long-lived project thread becomes expensive to reopen, CSM can distill its effective history into a lighter successor session with a much smaller cold-start footprint. Distill supports three compression levels: `lossless`, `balanced` (default), and `aggressive`.
4. **🧪 First-Request Preview**
CSM can reconstruct the next request Codex will actually send on resume, including rebuilt history, base instructions, and built-in tool schema estimates, so you can understand cold-start cost before opening a heavy thread.
5. **🔍 Chat History X-Ray (Inspection)**
Get a clear, transparent view of every thread: see exactly which model was used, how many tokens were consumed, and your current context footprint.
6. **🛡️ Completely Safe (Safe-by-default)**
Every operation in CSM strictly follows native Codex rules. It will never blindly rewrite history or corrupt your original chat logs.

---

## 🚀 Recommended Workflow: The `smart` Wizard

If you don't want to memorize complex commands, you only need to remember one: `smart`.

**The `smart` mode is the heart of CSM.** Just point it to a conversation, and it will pop up a guided menu letting you pick a new provider or model. It then automatically figures out the best path forward: whether to repair your current thread in place, smoothly migrate your history to a new one, or distill a lighter successor with a selectable compression level.

By default, `smart` and related flows do **not** write new profiles into `config.toml`. Profile writes are opt-in only when you explicitly pass `--write-profile`.

```powershell
# Launch the smart wizard (replace <ID> with your thread ID or path)
cargo run -- smart <thread-id-or-path>

```

If your main problem is that a single project thread has become too heavy to
resume efficiently, use `distill` to generate a lighter successor session from
the source thread's effective history:

```powershell
cargo run -- distill <thread-id-or-path>
```

Compression guidance:
- `lossless`: keeps much more of the thread's corrections, conventions, and active context
- `balanced`: the new default; cuts weight while still preserving implementation-safe detail
- `aggressive`: closest to the old ultra-compact behavior

---

## 💻 Visual Interface (TUI) Guide

Simply run `cargo run --` to enter the visual interface. The system will automatically detect your computer's language (supports English/Chinese).

* **Main Screen**: Your active and archived threads are grouped by **persisted provider** (the provider recorded in the rollout metadata). This keeps list grouping separate from the provider you may choose later at runtime.
* **Action Menu**: Press `Enter` on any thread to open its dedicated action menu (where you can run `smart` migration, repair, rename, etc.).
* **Shortcuts**:
* `↑ / ↓`: Move up/down
* `Enter`: Open Action Menu / Confirm
* `Esc`: Go back
* `r`: Reload list
* `F2`: Toggle English/Chinese
* `q`: Quit



---

## 🛠️ CLI Cheat Sheet (For Power Users)

For those who prefer the command line, CSM offers a rich set of direct commands:

### 🟢 Daily Management

* **Inspect a thread**: `cargo run -- show <ID>` (add `--json` for raw output)
* **Preview the first full request**: `cargo run -- first-token-preview <ID>` (reconstructs the next resume request and estimates rebuilt history, base instructions, tool schema, and total prompt size)
* **Rename a thread**: `cargo run -- rename <ID> "My New Project Chat"`
* **Copy resume link**: `cargo run -- copy-deeplink <ID>` (Copies the canonical `codex resume...` command so you can paste it in another terminal or send it to a teammate)
* **Archive / Unarchive**: `cargo run -- archive <ID>` or `unarchive <ID>`

### 🟡 Advanced & Migration

* **Smart switch wizard**: `cargo run -- smart <ID>` (Pick provider, model, execution mode, and distill compression level from the interactive picker)
* **Branch a thread (Fork)**: `cargo run -- fork <ID> --provider openrouter --model gpt-5` (Creates a new thread from the current one using a new model)
* **Slim down history (Compact)**: `cargo run -- compact <ID>` (Compresses chat history to free up context space)
* **Manual Migration**: `cargo run -- migrate <ID> --provider ...` (Designed for moving from large-window models to smaller ones; automatically compacts and forks)
* **Create a distilled successor**: `cargo run -- distill <ID> --compression-level balanced` (Builds a deterministic handoff brief and starts a lighter successor session)

### 🔴 Emergency Repair

* **Rebuild metadata**: `cargo run -- repair <ID>` (Use this when the info in your chat list doesn't match the actual files on disk)
* **Fix resume state**: `cargo run -- repair-resume-state <ID> --context-window 258400` (Use this in-place repair when an old thread won't open because of outdated context-window data)
* *Note: For heavy-duty metadata surgery, you can use the `rewrite-meta` command.*

---

## 💡 Under the Hood (For Developers)

While CSM provides a user-friendly high-level wrapper, it isn't "magic." It **does not** invent hidden thread states, nor does it fake migrations by blindly rewriting history.

* **Real Data Sources**: It operates directly on real Codex rollout files and config states under `$CODEX_HOME`.
* **Native Semantics**: It intentionally reuses Codex's internal Rust core logic (e.g., `ThreadManager::fork_thread`, `Op::Compact`).
* **Architecture**: Core operations live in `src/operations.rs` (native actions) and `src/rollout_edit.rs` (JSONL surgery), orchestrated safely by `src/commands.rs`.
* **Prompt Reconstruction**: `first-token-preview` and prompt-based `distill` rebuild the model-visible resume context from rollout history before estimating or generating successor handoff briefs.
