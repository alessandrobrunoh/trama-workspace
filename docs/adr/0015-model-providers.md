# ADR 0015: Model providers and the Settings area

- Status: Accepted
- Date: 2026-10-14

## Context

Every turn ran through the local Claude Code CLI (ADR 0002, ADR 0008). Users
asked to talk to other models in a thread: OpenAI, OpenRouter, local servers
such as Ollama and LM Studio, Groq. They also needed one place for settings
(appearance, keybindings, collaboration account, providers).

Constraints:

- ADR 0008 still holds. Claude.ai credentials are never read, stored or sent
  anywhere, and Trama does not route subscription usage on anyone's behalf.
- API billing must only happen for a provider the user added on purpose, with
  their own key.
- The agent features (tools, file edits, terminals, approvals, subagents,
  forks with checkpoints, land/review) are built on Claude Code's stream-json
  protocol. Re-implementing a tool loop for other models is out of scope.

## Decision

### Providers

- A **provider** is one of:
  - `claude_code`: built in, always present, cannot be edited or removed.
    It is the default provider and keeps today's behaviour.
  - `open_ai`: `https://api.openai.com/v1`, API key required.
  - `open_ai_compatible`: base URL, optional key, model. This one kind covers
    OpenRouter, Groq, Together, Ollama (`http://localhost:11434/v1`),
    LM Studio (`http://localhost:1234/v1`), vLLM and so on. The Settings UI
    offers presets that fill in name, URL and a model.
  - The Anthropic API is not a separate kind yet (see Not done). It can be
    used through an OpenAI-compatible gateway such as OpenRouter.
- The **daemon owns providers**. Settings live in `<app_dir>/providers.json`
  without secrets. API keys are stored in the OS keychain (service `trama`,
  account `provider:<id>`), with a `provider-keys.json` (0600) fallback
  when no keychain is reachable or `TRAMA_PROVIDER_KEY_FILE=1` is set. This
  is the same policy as the E2EE at-rest key. Keys never go back to clients:
  `ProviderSnapshot.has_api_key` only says whether a key exists.
- **Protocol v20** adds `ListProviders`, `SaveProvider { provider, api_key:
  keep | set | clear }`, `RemoveProvider`, `SetDefaultProvider`,
  `TestProvider` (a stored provider, or unsaved form values), and the replies
  `Providers` and `ProviderTestResult`. A test calls `GET {base}/models`.
  When it succeeds for a stored provider, the model list is saved and offered
  in the model picker.
- **Per-thread selection reuses the thread model.** A qualified model id
  `"<provider_id>::<model>"` selects an API provider. A plain id (for example
  `claude-opus-5-5`) is a Claude Code model, so existing threads, the DB
  schema and `SetThreadModel` stay as they were. `ModelInfo.provider` names
  the provider for picker rows. A thread with no override uses the default
  provider's default model when the default is an API provider. Otherwise it
  uses Claude Code with the workspace default model, as before.
- **Routing.** `start_turn` resolves the thread's route. Claude Code turns
  are unchanged. API turns store the prompt the same way (comments and
  mentions included), then stream a plain chat completion. The provider gets
  a short system prompt, the thread's text history (the last 60 messages,
  about 400k characters at most) and the prompt. Deltas go out as `TextDelta`
  events and the answer is stored as an assistant message, so the transcript,
  comments and collab mirroring work as usual. Interrupt cancels the stream.
  A removed provider makes the prompt fail before anything is stored. It
  never falls back to Claude silently.
- **API turns are chat only.** They run no tools, read no files and start no
  sessions, approvals or subagents. The Settings > Providers page and every
  picker row say "chat only, no tools or files".
- **Client.** `trama_infra::llm::OpenAiCompatClient` is a streaming
  chat-completions client: hyper HTTP/1.1, rustls with system roots (no new
  TLS stack), and an incremental SSE parser. Transport policy: `https://` to
  any host, always verified. `http://` is allowed for loopback, and for other
  hosts only when no key is set (a LAN Ollama), so a key never crosses a
  network in clear text. Error messages and `Debug` output never contain the
  key.

### Settings

- The desktop keeps a `settings.json` in the platform config dir: macOS
  `~/Library/Application Support/TramaWorkspace`, Windows
  `%APPDATA%\TramaWorkspace`, Linux `$XDG_CONFIG_HOME/TramaWorkspace`
  (default `~/.config/TramaWorkspace`). `TRAMA_DATA_DIR` overrides it.
  Unknown or invalid values fall back to their defaults.
- The Settings view (palette: "Settings", default binding `secondary-alt-,`)
  has a sidebar with General, Appearance, Keybindings, Collaboration,
  Providers and Advanced.
- Keybindings are stored with `secondary` (⌘ on macOS, Ctrl elsewhere), so one
  file means the same thing on every OS. Overrides apply live: keys that
  were bound before are masked with `NoAction`, then the effective table is
  bound (in GPUI, later bindings win). Recording a key that another action
  already uses is refused, with the name of that action.

## Consequences

- Users can chat with any OpenAI-compatible model in a thread and switch back
  to Claude Code from the model picker at any time. The history carries over,
  as text only.
- API usage is billed only to keys the user entered. Nothing changes for
  Claude Code billing (ADR 0008).
- A shared thread whose sender picked an API provider runs that turn on the
  sender's machine with the sender's key, the same as Claude Code turns.

## Not done yet

- A native Anthropic Messages API provider.
- Tool use for API providers.
- Applying density to every list row. The value is saved and offered in the
  UI, but only some views read it.
- The shortcuts sheet still shows the default bindings, not the overrides.
- Per-turn usage and cost accounting for API providers.
