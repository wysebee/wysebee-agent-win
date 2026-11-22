# Wysebee Agent for Windows

A small app in the Windows notification area (the system tray) that lets **one Wysebee
agent run the programs you allow on this PC**, and the scripts of the skills you allow, and
**find, read and (where you say so) write files in the folders you pick here**.

It is the Windows build of Wysebee Agent for Mac and does the same
things the same way: it pairs with Wysebee Cloud, is handed calls for the programs one
agent's settings hold, and runs each **only after you allowed it here, on the PC**. It runs
no model. It runs with your own rights (no administrator, no service) and is installed for
you alone.

It speaks Wysebee Cloud's agent-link protocol (shared with the Mac app), pairing as the agent
type `wysebee-windows`.

## How it works

1. Open Wysebee Agent, type a name for a new agent, click **Pair**. It shows a code
   (`XXXX-XXXX`).
2. In Wysebee, under **Devices**, type the code: on the website (the agent then lives in
   Wysebee Cloud, which runs it, so the PC needs no Wysebee Desktop) or in Wysebee Desktop
   (it runs there). Pairing creates a new agent; if the name is taken by an agent of this kind with no device now,
   Wysebee offers to reconnect to it instead (nothing is allowed until you click Allow here
   again). Only that agent runs programs on
   this PC.
3. In Wysebee, on that agent's **Tools** tab, add a "Run local program" tool: a program,
   its fixed arguments and the blanks the agent fills.
4. The program shows up in Wysebee Agent's window with its whole command line, **waiting**.
   Click **Allow**. Change it in Wysebee later and it waits again.
5. The agent can now run it. The switch "Let Wysebee run programs on this PC" pauses
   everything; **Unpair** ends it.

### Skills

Wysebee Desktop no longer runs a skill's scripts: they run **only for an agent paired with
this app, and only here**. Give the agent a skill in Wysebee (its Skills tab); it shows up
under **Skills** in this window, **waiting**, with the scripts its `SKILL.md` declares (each
command line) and its files. As soon as a skill is given to the agent, or changed in
Wysebee, the app downloads its files from Wysebee Cloud, checks each one against its
SHA-256 and keeps them on this PC ("Downloaded to this PC") — nothing of it runs yet.
Click **Allow**: the app puts them in the skill's folder here and finds each script's
program. Change any file of the skill in Wysebee and it is downloaded again at once, and
waits for Allow again; until then the copy you allowed stays as it was, and nothing of the
new one runs.

**Show in File Explorer** opens the skill's folder on this PC. Files you put there yourself (a
`.env` the scripts read, say — Wysebee never syncs files that look like secrets) **stay on
this PC and are never sent**; an Allow replaces only the skill's synced files.

### Folders

Local Folder moved here from Wysebee Desktop (a sandboxed app cannot open a folder path
somebody typed). Under **Folders**, click **Add a folder…**: the PC's own folder picker opens.
Give it a name (the folder's own name if you leave it empty), choose **Read only** or **Read
and write**, and say what it holds if you like. The agent's folder tools (`folder_find`,
`folder_list`, `folder_read`, `folder_write`) then work in it: text, Word (`.docx`), Excel
(`.xlsx`, `.xlsm`, `.xls`, `.ods`) and PDF are read as text; a picture (`.png`, `.jpg`,
`.jpeg`, `.gif`, `.webp`) is shown to you in Wysebee and never to the model. **Allow writing**
/ **Make read only**, **Edit** and **Remove…** change it later. The folders are numbered here
as the agent sees them. There is nothing to approve: you chose each one here.

## The security model

- **You name the program and its fixed arguments; the model only fills blanks.** A call
  from the cloud carries a program's *name* and the blanks' values, nothing else. What runs
  is this PC's own approved copy: the program's absolute path as resolved when you clicked
  Allow, with the arguments you saw.
- **Never a shell.** The program is started with an argument list; a value is one
  argument, whatever it holds (`a b; rm -rf / $(x)` is just that text).
- **Approval is local.** It is kept in this app's state file and never taken from the
  cloud. A program is the same while its name, command and arguments (each fixed value and
  each blank's name, in order) are unchanged; its description is not part of it. Anything
  else changed: it waits again and is refused until allowed. Removed in Wysebee: its
  approval is lost.
- **Bounded.** No input, 60 seconds (then it is killed), output and errors cut at 20,000
  characters each, run in your home folder.
- **Skills: only declared scripts.** A `skill_run` call carries a skill's name, a script's
  name and up to 30 arguments. What runs is the command that script has in the approved
  copy's `SKILL.md`, then the arguments, each one argument (never a shell), in the skill's
  folder here, with no input, for at most 60 seconds, output cut as above. A script's
  program is the first word of its command: a path with `/` is a file inside the skill's
  folder (a link leading out, or `..`, is refused, at Allow and again at each run), any
  other word a program on the app's `PATH` (plus the app execution aliases' and Scoop's folders), found when you allow.
  A skill is the same while its synced files (each path and SHA-256) are unchanged; a file
  that does not match its SHA-256 is refused and nothing is allowed. A skill no longer
  given to the agent loses its approval; its folder stays until you unpair.
- **Folders: only the ones you picked, only as you allowed.** A `folder_call` names a tool,
  a folder by the id this app gave it and the model's arguments. A folder this app does not
  have is refused; a path is relative to the folder, never absolute, never `..`, and is
  resolved through links and must still be inside the folder; writing needs a folder you
  made read and write, whatever the call says. Bounded as Wysebee Desktop's tools were:
  files read up to 50 MB (a picture 10 MB), a search over at most 50,000 entries answering
  200, a listing 500 entries, sheets 5,000 rows, a write 1,000,000 characters, every answer
  cut at 50,000 characters; `.git`, `node_modules`, `.Trash` and `__pycache__` are skipped by
  a search. Paused, nothing is answered.
- **Run any program: off unless you turn it on.** An agent given "Run any program" in Wysebee
  may name any program and its arguments itself (`program_run`), but this PC runs it only while
  you have turned on **Let Wysebee run any program on this PC** here (it asks you to confirm,
  and unpairing turns it off). It then runs as an allowed program does: an `.exe` or `.com`,
  never a shell or batch file, no input, 60 seconds, output cut, in your home folder.
  Even then a list of dangerous programs is refused: disk, boot and backup tools (`format`,
  `diskpart`, …), shells and script hosts (`cmd`, PowerShell, …), tools that change accounts,
  permissions, services, software or logs, shutting down, deleting files outright, inline
  code (`python -c`), and git/robocopy arguments that destroy data. It is a safeguard, not a
  sandbox: a program it lets through can still do what that program does.
- **Screen control: off unless you turn it on.** An agent given "Screen control" in Wysebee may
  take screenshots of this PC's main display and use the mouse and keyboard, but only while you
  have turned on **Let Wysebee see and control this PC's screen** here (confirmed, off when you
  unpair; Pause stops it). A red ring shows where it clicks. Nothing checks what it types or
  clicks: it can do what you could do at the keyboard, so turn it on only while you want it.
- **Developer access: off unless you turn it on.** An agent given "Developer access" in Wysebee
  may run PowerShell and cmd commands and read, write, edit and search files **anywhere on this
  PC, as you** — like a coding assistant running here — but only while you have turned on
  **Let Wysebee use this PC as a developer** (confirmed, off when you unpair). Commands wait up
  to 220 seconds; longer ones (builds) run as background jobs, which Pause stops. A shell can do
  anything you can, so the list of refused programs does not apply here.
- **The Android Development Toolkit: off unless you turn it on.** With **Let Wysebee use
  Android phones on this PC** on, an agent given the toolkit uses the phones and emulators adb
  sees here (adb from Android Studio's SDK or on your PATH): screenshots, logcat, taps and
  typing, installing apps, `adb shell` — never root or the bootloader.
- **Eleven operations only**: `ping`, `local_run`, `skill_run`, `folder_call`, `program_run`,
  `screen_screenshot`, `screen_action`, `dev_shell`, `dev_job`, `dev_file` and `android_dev`.
  Anything else is refused.
- **The link's secret** (32 random bytes) is made here and kept in the Windows Credential Manager
  (a generic credential, service `com.wysebee.agent.win`); the cloud keeps only its SHA-256. It is sent only to
  the cloud the pairing was made with, and is never in the state file or the log.
- **The cloud's address** must be `https://`, or plain `http://` to this computer.
- **What Wysebee Cloud sees**: the program definitions (they are the agent's settings),
  the skills' synced files (it holds them), each call's blank values or script arguments
  and the output, kept until read, an hour at most. It also sees your folders' names, paths,
  permissions and descriptions (it puts them in the agent's settings), and what a folder tool
  read or wrote passes through it like any answer — a picture too. Nothing else is uploaded
  from this PC.

## What is different on Windows

Everything above holds as on the Mac; these are the places Windows differs.

- **A program is an `.exe` or `.com` file.** A bare name is looked for on the `PATH` as
  `name`, `name.exe`, `name.com` (so `python` is `python.exe`), in the folders of the app's
  `PATH`, then `%LOCALAPPDATA%\Microsoft\WindowsApps` (app execution aliases: `winget`,
  Store Python) and `%USERPROFILE%\scoop\shims`.
- **Never a batch file.** A `.cmd` or `.bat` file runs through `cmd.exe`, a shell, which
  this app never starts: Allow refuses one and says so (`npm` is `npm.cmd`: choose the
  program it starts, `node.exe`, instead). A script runs through its interpreter: put
  `python`, `node` or `pwsh` as the program and the script as a fixed argument.
- **A skill's own program** (`./bin/tool.exe` in its folder) must be an `.exe` or `.com`
  too; a declared script like `./run.py` is refused at Allow with a sentence. Declare it as
  `python run.py`. Paths in `SKILL.md` use `/`, as everywhere in the protocol.
- **No console window**: programs are started with `CREATE_NO_WINDOW`, so a console
  program does not flash a window.
- **The secret** is a generic credential in the Windows Credential Manager (target
  `link-secret.com.wysebee.agent.win`), readable only by you.
- **Paths the tools answer** (`folder_find`) use `/`, as Wysebee Desktop's did; a path the
  model gives may use `/` or `\`. A full path (`C:\…`, `\\server\share`, `\…`) or `..` is
  refused, as on the Mac.
- **The agent type** is `wysebee-windows` "Wysebee Windows Agent". A cloud that does not know
  it answers `/start` with `invalid_kind`.

## Build, run, test

**Windows 10 or 11, x64.** Every script is PowerShell (5.1 or 7) and runs on Windows only.

Install once:

1. **Rust** with the MSVC toolchain: <https://rustup.rs> (the default,
   `stable-x86_64-pc-windows-msvc`).
2. **Visual Studio C++ Build Tools** (the "Desktop development with C++" workload): the
   linker and the C compiler some crates need.
3. **Microsoft Edge WebView2 Runtime**: on Windows 11 already; on Windows 10 from Windows
   Update or <https://developer.microsoft.com/microsoft-edge/webview2/>.
4. For `scripts\check.ps1`: `cargo install cargo-llvm-cov`, and **Developer Mode** on
   (Settings > System > For developers), so the tests can make file links.

The window is plain HTML/CSS/JS in `ui\`, with no build step.

```powershell
cargo build --workspace
cargo test --workspace                 # agent-core against a fake cloud on 127.0.0.1; nothing real is touched
scripts\check.ps1                      # before each commit: clippy, every test, 100% of agent-core's lines
cargo run -p agent-core --example headless -- <state dir> "<agent name>"
                                       # no window: <state dir>\allow stands in for the Allow
                                       # clicks and the folder picker (folder:<path>, folder-write:<path>)
cargo run -p agent-app                 # the app: an icon in the notification area and its window
```

If PowerShell refuses to run a script ("running scripts is disabled on this system"):

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
```

### For development: `scripts\run_dev.ps1`

```powershell
scripts\run_dev.ps1                     # from source, against Wysebee Cloud (https://app.wysebee.com)
scripts\run_dev.ps1 -Url <address>      # against another cloud
scripts\run_dev.ps1 -Fresh              # forget the dev state first
scripts\run_dev.ps1 -Headless "Dev agent"   # the headless example instead of the window
```

It sets `WYSEBEE_CLOUD_URL` and a state folder of its own per cloud
(`%LOCALAPPDATA%\Wysebee Agent Dev\<prod|custom>\`), so it runs beside an installed copy, paired
as an agent of its own; a debug build has a console, and the log goes there. Each run
recompiles the app crate, so the window is always built from `ui\` as it is now (new files
too).

By hand:

```powershell
$env:WYSEBEE_CLOUD_URL = 'https://<your cloud>'
$env:WYSEBEE_AGENT_STATE_DIR = "$env:TEMP\wysebee-agent-dev"   # optional: apart from an installed copy
cargo run -p agent-app
```

Or, while unpaired, enter the address in the window's **Server** tab (https, or http to
this computer); the saved address wins over the variable, which is only the default ("Use the
default" goes back to it, else to `https://app.wysebee.com`).
A state folder other than the default has a Credential Manager item of its own, so the two
copies' pairings never replace each other.

### A release build

```powershell
cargo build --release --locked -p agent-app --features custom-protocol --target x86_64-pc-windows-msvc
# -> target\x86_64-pc-windows-msvc\release\wysebee-agent.exe
```

The app is one `.exe` with nothing beside it: the window, the notification area icon and the
app's icon are compiled in. `custom-protocol` serves the embedded page as a production build
does. A build of your own is unsigned, so SmartScreen warns on another PC; Wysebee's signed
installer (per-user, no administrator, `%LOCALAPPDATA%\Programs\Wysebee Agent`) is built
outside this repository.

## Where things live

| What | Where |
| --- | --- |
| State: the pairing (link id, agent, cloud address), the pause switch, the agent's programs as the cloud last sent them, what you allowed, the folders you picked (forgotten when you unpair) | `%LOCALAPPDATA%\Wysebee Agent\state.json` |
| The link's secret | Windows Credential Manager, generic credential, service `com.wysebee.agent.win`, account `link-secret` (another state folder: `link-secret <16 hex digits of the folder path's SHA-256>`) |
| The approved copy of each skill (its synced files, and any you put there) | `%LOCALAPPDATA%\Wysebee Agent\skills\<name>\` |
| Skill files downloaded and checked, by SHA-256 (shown while a skill waits; never run) | `%LOCALAPPDATA%\Wysebee Agent\skill-cache\` |
| The log (truncated per launch; to the console instead in a debug build) | `%LOCALAPPDATA%\Wysebee Agent\agent.log` |
| Start at sign-in | the value `Wysebee Agent` under `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` (the exe's path, quoted) |
| The installed app | `%LOCALAPPDATA%\Programs\Wysebee Agent\Wysebee Agent.exe` |

## Layout

```
Cargo.toml          the workspace; the version lives here and only here
crates/agent-core   everything bar the window, and every test: programs.rs, skills.rs,
                    folders.rs, approvals.rs, secrets.rs (+ secrets/credentials.rs), cloud.rs,
                    link.rs, agent.rs; tests/ against a fake cloud, tests/support/program.rs
                    the tests' stand-in program
crates/agent-app    the Tauri 2 shell: main.rs (window, commands), tray.rs (notification area),
                    autostart.rs (the Run key)
ui/                 the window: index.html, style.css, app.js
scripts/            run_dev.ps1, check.ps1, common.ps1 (PowerShell, Windows only)
```
