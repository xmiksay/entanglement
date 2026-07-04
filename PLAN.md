Here is the top-level plan for Project `entanglement`, structured as a Markdown document you can drop directly into a `PLAN.md` file at the root of your repository.

```markdown
# Project Entanglement: Architecture & Development Plan

## 1. Vision
**Entanglement** is a headless, Rust-based AI coding agent engine. It decouples the "reasoning and tool-execution" loop from the user interface. Once built, `entanglement` can be driven by a terminal CLI, a web interface, or any other harness simply by swapping out the transport layer.

## 2. System Architecture
The project follows a strict **Headless Core + Transport Trait** pattern to ensure maximum reusability.

```text
[ UI / Harness ]                        [ entanglement-core ]
       |                                       |
       | 1. UserAction (Prompt, Approve)       | 3. Anthropic API
       | ------------------------------------> | 4. Tool Execution (FS/Bash)
       |                                       |
       | 5. AgentEvent (Text, ToolReq, Done)   | 2. Context Management
       | <------------------------------------ |
```

### 2.1 Core Components
*   **`entanglement-core`**: The agnostic reasoning engine. Manages LLM context, streams API responses, executes local tools (Read/Write/Bash), and enforces the event loop. **No UI/CLI/Web dependencies allowed.**
*   **`Transport Trait`**: The asynchronous contract between the Entanglement and the outside world. Defines how actions flow in and events flow out.
*   **`entanglement-cli`**: A thin terminal wrapper. Translates `stdin` to `UserAction` and renders `AgentEvent` to `stdout` (via gRPC/WebSocket client).
*   **`entanglement-web`**: (Future) A thin web wrapper. Translates frontend WebSocket messages to the Transport Trait.

## 3. Data Contracts (The Transport Protocol)

### Inbound: `UserAction`
*   `SendPrompt(String)`
*   `ApproveTool(String)` (Human-in-the-loop consent)
*   `RejectTool(String)`
*   `Cancel`

### Outbound: `AgentEvent`
*   `Thinking`
*   `TextDelta(String)` (Streaming text chunks)
*   `ToolUseRequest { id: String, tool: String, input: String }` (Pauses loop, waits for approval)
*   `ToolOutput(String)` (Result of the executed tool)
*   `Error(String)`
*   `Done`

## 4. Repository Structure
```text
entanglement/
├── PLAN.md
├── Cargo.toml              # Workspace root
├── entanglement-core/             # The Headless Engine
│   ├── Cargo.toml          # deps: tokio, reqwest/async-anthropic, serde
│   └── src/
│       ├── lib.rs
│       ├── engine.rs       # The main Entanglement<T> struct & loop
│       ├── transport.rs    # Trait definitions
│       ├── llm.rs          # Anthropic API client wrapper & SSE parsing
│       ├── tools.rs        # Filesystem & Bash exec implementations
│       └── context.rs      # Token counting & message history trimming
├── entanglement-cli/              # Terminal Head
│   ├── Cargo.toml          # deps: tokio, tonic/axum, crossterm
│   └── src/
│       └── main.rs         # CLI parser, connects to core via transport
└── entanglement-web/              # (Phase 3) Web Head
    └── ...
```

## 5. Development Phases

### Phase 1: The Dummy Engine (Foundation)
**Goal:** Prove the core loop works without networking or real UI.
*   [ ] Initialize Cargo workspace and sub-crates.
*   [ ] Define `UserAction` and `AgentEvent` enums in `entanglement-core`.
*   [ ] Implement the `Transport` trait in `entanglement-core`.
*   [ ] Create a `DummyTransport` (prints events to console, auto-approves tools).
*   [ ] Write the basic `Holly::think()` loop.
*   [ ] Integrate `async-anthropic` / `reqwest` to stream Claude 3.5 Sonnet responses.

### Phase 2: Tooling & Context (Capabilities)
**Goal:** Allow Entanglement to actually read/write code and manage memory.
*   [ ] Implement `ReadFile` tool.
*   [ ] Implement `WriteFile` tool.
*   [ ] Implement `Bash` tool (using `tokio::process::Command`).
*   [ ] Build Context Manager: Track token usage (approximation), implement sliding window or summarization when approaching 200k limit.
*   [ ] Implement Git safety net: Auto-commit before `WriteFile` executes.

### Phase 3: The Real Transport (Connecting the World)
**Goal:** Replace `DummyTransport` with a real bi-directional protocol.
*   [ ] Choose protocol (Recommendation: gRPC via `tonic` for Rust-to-Rust speed, or WebSockets via `axum` for easier web integration later).
*   [ ] Implement `GrpcTransport` (or `WsTransport`) in `entanglement-core`.
*   [ ] Spin up the transport server *inside* `entanglement-core`.

### Phase 4: The CLI Head (First Real Interface)
**Goal:** A usable terminal application.
*   [ ] Build `entanglement-cli` binary.
*   [ ] Connect client to the `entanglement-core` server.
*   [ ] Implement terminal rendering (spinning loaders, colored text).
*   [ ] Implement terminal input for `ApproveTool` / `RejectTool` (e.g., pressing `y` or `n` when Entanglement wants to run bash).
*   [ ] Add graceful shutdown (Ctrl+C handling).

### Phase 5: Polish & Ecosystem
**Goal:** Make it production-grade.
*   [ ] Add robust error handling for malformed LLM JSON outputs.
*   [ ] Implement diffing (Search/Replace blocks instead of full file rewrites).
*   [ ] Build `entanglement-web` (Node/Go/Rust frontend wrapper connecting to the same transport).
*   [ ] Configuration file (`~/.entanglementrc` for default model, API keys, auto-approve rules).

## 6. Key Technical Risks & Mitigations

| Risk | Mitigation |
| :--- | :--- |
| **SSE Stream Parsing** Claude sends fragmented JSON for tool inputs. | Buffer `content_block_delta` events until `content_block_stop`, then parse the whole JSON string. |
| **Token Count Inaccuracy** Anthropic uses a specific tokenizer not easily available in Rust. | Use a safe heuristic (e.g., `chars / 3.5`) and set the context limit slightly below 200k (e.g., 180k) to prevent hard API crashes. |
| **Bash Process Hanging** A tool command might block indefinitely. | Implement async timeouts on `tokio::process::Command` (e.g., kill after 60 seconds). |
| **Strict Dependency Hygiene** Accidentally coupling core logic to CLI crates. | Enforce via `cargo tree -p entanglement-core` during CI to ensure no `clap`, `crossterm`, etc., are linked. |
```
