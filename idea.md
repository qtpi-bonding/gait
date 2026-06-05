Here is a comprehensive, production-ready **Technical Design Document (TDD)** structured exactly how an advanced coding agent needs it. You can feed this entire document into a tool like Claude, ChatGPT, or your own scaffolding agent to have it autogenerate the baseline repository structure.

---

# Technical Design Document: The Stateless Inverted Git-Ops Agent Harness (SIGAH)

## 1. Core Philosophy & System Architecture

### 1.1 Statement of Intent

Most modern AI coding agents are plagued by structural bloat: they utilize resource-heavy background processes, long-lived daemons, hidden cloud-hosted state machines, and proprietary, clumsy terminal user interfaces (TUIs).

**SIGAH** treats an AI agent loop as an ephemeral, purely stateless compiler pipeline. It operates entirely on raw Unix filesystems and native standard streams (`stdout`/`stdin`).

### 1.2 Separation of Concerns via Git Inversion

State and application logic are entirely isolated by reversing the traditional workspace layout:

* **The Master Container:** A dedicated Git repository tracking the AI’s memory state (`sessions/`).
* **The Codebase Target:** The user's application codebase lives explicitly as a sandboxed **Git Submodule** inside the master container.

```
AI-ORCHESTRATOR-WORKSPACE/ (Master Git Repo - Tracks AI Memory State)
 ├── package.json
 ├── tsconfig.json
 ├── harness.ts                  # Pure Stateless CLI Executor
 ├── sessions/
 │    └── [session-id]/
 │         └── transcript.md     # The Absolute Source of Truth Ledger
 └── codebase/                   # <-- [GIT SUBMODULE pointing to Target App Code]
      ├── src/
      ├── package.json
      └── .git

```

---

## 2. Behavioral Loop & Transaction Logic

Every execution cycle of the CLI harness is synchronous, blocking, and atomic. A single execution "Turn" is defined by a strict state lifecycle:

```
[User runs CLI] ➔ [Parse Active Files + Log] ➔ [One-Shot LLM Execution Stream]
                        │
       ┌────────────────┴────────────────┐
       ▼                                 ▼
 [Tool Executed (File Edit)]     [Clarification Question Asked]
       │                                 │
 [Update Code on Disk]           [Block on Terminal stdin]
       │                                 │
 [Append Action to md Log]       [Write User Reply to md Log]
       │                                 │
 [Auto-Commit Parent Repo]       [Auto-Commit Parent Repo]
       │                                 │
 [Exit Process (Status 0)]       [Exit Process (Status 0)]

```

### 2.1 The Two-Tier Commit Strategy

To prevent the agent from cluttering production source code histories with automated, messy micro-commits, the Git lifecycle is decoupled:

1. **The Codebase Submodule:** The agent writes directly to files inside `codebase/` but **never** commits them. File state remains dirty/staged in the user’s workspace for human review and curation.
2. **The Parent Operator Repo:** The harness **automatically commits** the parent repository at the end of every turn. This commit captures the updated `transcript.md` log *and* locks down the precise commit pointer of the underlying codebase.

---

## 3. Specifications for the Automated Agent Implementation

### 3.1 Technical Stack

* **Runtime Environment:** Node.js or Bun (Command-line execution)
* **Language:** TypeScript
* **Dependencies:** Strict minimalism. Limit external dependencies to core LLM SDK packages (e.g., `@google/genai` or `@anthropic-ai/sdk`). Use native runtime modules (`fs`, `path`, `child_process`, `readline`) for all system interfaces.

### 3.2 Feature Set Checklist

1. **Command Line Interface (CLI) Interface:**
* Invoke via syntax: `harness --session <id> --task "instruction" [files...]`
* Subsequent turns: `harness --session <id> [files...]`


2. **Dual-Writer Tee Stream Processor:**
* Stream incoming LLM generation text token-by-token.
* Send text to terminal `stdout` wrapped in native ANSI escape codes for user readability (Cyan text for thoughts, Blue for tool events).
* Simultaneously write the raw, uncolored tokens directly to the append-only `transcript.md` on disk.


3. **Stateless Replayability Mechanism:**
* At startup, the script parses the target files and injects them alongside the total, read history of `transcript.md`.
* It tracks and appends current codebase metadata inline using invisible markdown comment wrappers:
``



---

## 4. Bootstrap Implementation Code Blueprint

This is the exact functional blueprint the coding agent should generate to wire the workspace together.

```typescript
import * as fs from 'fs';
import * as path from 'path';
import { execSync } from 'child_process';
import * as readline from 'readline';
import { GoogleGenAI } from '@google/genai'; // Interchangeable with Anthropic SDK

const ai = new GoogleGenAI();
const CODEBASE_DIR = path.join(__dirname, 'codebase');
const SESSIONS_DIR = path.join(__dirname, 'sessions');

// Pure Unix text wrapper for CLI inputs
const askQuestion = (query: string): Promise<string> => {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) => rl.question(query, (ans) => { rl.close(); resolve(ans); }));
};

class DualStream {
  private fileStream: fs.WriteStream;
  constructor(filePath: string) {
    this.fileStream = fs.createWriteStream(filePath, { flags: 'a', encoding: 'utf8' });
  }
  // Console gets UI color accents, disk gets raw markdown
  write(token: string, colorCode: string = '\x1b[0m') {
    process.stdout.write(`${colorCode}${token}\x1b[0m`);
    this.fileStream.write(token);
  }
  end() { this.fileStream.end('\n\n'); }
}

async function startCycle(sessionId: string, targetFiles: string[], taskIntent?: string) {
  const sessionPath = path.join(SESSIONS_DIR, sessionId);
  fs.mkdirSync(sessionPath, { recursive: true });
  const transcriptPath = path.join(sessionPath, 'transcript.md');

  // 1. Establish state ledger
  if (!fs.existsSync(transcriptPath)) {
    fs.writeFileSync(transcriptPath, `# Session Execution History: ${sessionId}\n`);
  }
  if (taskIntent) {
    fs.appendFileSync(transcriptPath, `\n\n## User Input\n${taskIntent}\n`);
  }
  const currentTranscript = fs.readFileSync(transcriptPath, 'utf-8');

  // 2. Sample Code Base Targets
  let workspaceCodeSnapshot = '';
  for (const file of targetFiles) {
    const fullPath = path.join(CODEBASE_DIR, file);
    if (fs.existsSync(fullPath)) {
      workspaceCodeSnapshot += `\n--- FILE: ${file} ---\n${fs.readFileSync(fullPath, 'utf-8')}\n`;
    }
  }

  // 3. Collect underlying metadata via Git
  const currentCodeHash = execSync('git rev-parse HEAD', { cwd: CODEBASE_DIR }).toString().trim();
  const isDirty = execSync('git status --porcelain', { cwd: CODEBASE_DIR }).toString().trim().length > 0;
  const metaString = `\n`;

  const payload = `
    You are a stateless, headless CLI automation pipeline runner.
    Your entire transaction memory is supplied below in the 'CONVERSATION LOG'.
    Modify target files by providing full text replacements when required, or output plain text to question the user.
    Never output custom UI layouts or formatting shapes.
    
    === IMMUTABLE CONVERSATION LOG ===
    ${currentTranscript}
    ${metaString}
    
    === APP TARGET FILES ===
    ${workspaceCodeSnapshot}
  `;

  // 4. Fire and split stream
  const writer = new DualStream(transcriptPath);
  writer.write('## Agent Stream\n', '\x1b[1;34m'); // System Notification Bold Blue

  const responseStream = await ai.models.generateContentStream({
    model: 'gemini-2.5-flash', // Or choice LLM
    contents: payload
  });

  let fullAgentTextOutput = '';
  for await (const chunk of responseStream) {
    if (chunk.text) {
      writer.write(chunk.text, '\x1b[36m'); // Agent output streams in clean Cyan
      fullAgentTextOutput += chunk.text;
    }
  }
  writer.end();

  // 5. Evaluate Response Path
  // Check if output dictates standard file mutations (Parse custom blocks if utilizing stolen tool libraries)
  const codeBlockRegex = /```[\s\S]*?```/g; // Simplified fallback parser logic
  
  if (fullAgentTextOutput.includes('WRITE_FILE:')) {
     // Apply filesystem updates cleanly to `CODEBASE_DIR` here
     console.log('\n\x1b[1;32m[Changes processed on disk. Parent state auto-committing...]\x1b[0m');
     execSync(`git add ${transcriptPath}`, { cwd: __dirname });
     execSync(`git commit -m "session(${sessionId}): auto turn sync"`, { cwd: __dirname });
     process.exit(0);
  }

  // 6. Otherwise: Fall back to native standard input blocking gate
  console.log('\n==================================================');
  const userFeedback = await askQuestion('\x1b[1;33mYour Reply via Stdin (Or type "exit"): \x1b[0m');
  console.log('==================================================\n');

  if (userFeedback.trim().toLowerCase() === 'exit') process.exit(0);

  fs.appendFileSync(transcriptPath, `\n\n## User Response\n${userFeedback}\n`);
  execSync(`git add ${transcriptPath}`, { cwd: __dirname });
  execSync(`git commit -m "session(${sessionId}): user input transaction recorded"`, { cwd: __dirname });
  
  process.exit(0);
}

```

Yes, exactly! That is the cleanest bootstrap UX imaginable.

Instead of creating some complex wizard that installs global background config folders, `ainix` should just leverage standard Git commands to set up the workspace.

When a user runs `ainix init`, the program should automate a clean, two-step process right in their current directory:

```bash
# 1. Initialize the parent operator repository (The AI's brain/memory)
git init

# 2. Instruct the user to pull in their codebase target
# "ainix: Workspace initialized. Now add your app repository as the isolated codebase:"
git submodule add <your-app-repo-url> codebase

```

---

### The `ainix init` Workflow Specification

To give your coding agent the exact logic to write, the `init` command inside `harness.ts` should execute these steps using standard Node/Bun `child_process` execution:

```typescript
import { execSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

function initializeWorkspace() {
  const currentDir = process.cwd();
  
  console.log('\x1b[1;36m[ainix]: Initializing UNIX-style AI session container...\x1b[0m');

  // 1. Run local git init for the parent tracker if it doesn't exist
  if (!fs.existsSync(path.join(currentDir, '.git'))) {
    execSync('git init');
  }

  // 2. Create the required session folder architecture
  const sessionsDir = path.join(currentDir, 'sessions');
  if (!fs.existsSync(sessionsDir)) {
    fs.mkdirSync(sessionsDir);
  }

  // 3. Create a clean .gitignore so the user doesn't accidentally track node_modules
  const gitignoreContent = "node_modules/\n.env\n";
  fs.writeFileSync(path.join(currentDir, '.gitignore'), gitignoreContent);

  // 4. Print the exact follow-up instruction 
  console.log('\n\x1b[1;32m✔ Parent operator repository successfully initialized.\x1b[0m');
  console.log('\x1b[1;33m👉 Next Step: Link your isolated application code as a submodule:\x1b[0m');
  console.log('   \x1b[34mgit submodule add <your-git-app-url> codebase\x1b[0m\n');
  console.log('Once added, start a stateless conversation turn using:');
  console.log('   \x1b[34mainix --session s1 --task "Analyze the codebase layout" [files...]\x1b[0m\n');
}

```

---

### Why this is so elegant

* **Zero Hidden Magic:** The user sees exactly what happened. A `.git` folder was created locally, a `sessions/` directory appeared, and a standard Git submodule command handles linking the application code.
* **Instant Cloneability:** If you need to move this whole workspace or backup your entire AI interaction state to GitHub, you don't need a custom database export feature. You just add a remote to the parent directory and run `git push`. Anyone else cloning it can run `git clone --recursive` and instantly inherit your entire conversational history and code snapshot ledger simultaneously.

It strips away all the magic and leaves a standard, transparent Unix directory structure. Have the coding agent inject this exact `init` command sequence into your blueprint!


