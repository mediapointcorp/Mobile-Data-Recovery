// Child-process client for `photorecovery-worker serve` (JSON lines over
// stdio). No sockets are opened; only this process can talk to the worker.

import { ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";
import type { WorkerEvent } from "../shared/types";

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: NodeJS.Timeout;
}

const WORKER_METHODS = new Set([
  "tool.version",
  "drives.list",
  "case.open",
  "adb.status",
  "adb.devices",
  "adb.deviceInfo",
  "adb.listPartitions",
  "job.start",
  "job.resume",
  "job.cancel",
  "job.list",
  "results.finalize",
  "results.list",
  "export.run",
  "report.generate",
]);

export function locateWorker(resourcesPath: string, appPath: string): string {
  const exe = process.platform === "win32" ? "photorecovery-worker.exe" : "photorecovery-worker";
  const candidates = [
    process.env.PHOTORECOVERY_WORKER ?? "",
    join(resourcesPath, "bin", exe),
    join(appPath, "..", "target", "release", exe),
    join(appPath, "..", "target", "debug", exe),
  ].filter((p) => p.length > 0);
  const found = candidates.find((p) => existsSync(p));
  if (!found) throw new Error(`worker executable not found; build it with 'cargo build --release' (looked in: ${candidates.join(", ")})`);
  return found;
}

export class WorkerClient extends EventEmitter {
  private child: ChildProcessWithoutNullStreams | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor(private readonly exePath: string) {
    super();
  }

  start(): void {
    const child = spawn(this.exePath, ["serve"], { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.child = child;
    const rl = createInterface({ input: child.stdout });
    rl.on("line", (line) => this.onLine(line));
    child.stderr.on("data", (d: Buffer) => process.stderr.write(`[worker] ${d.toString()}`));
    child.on("exit", (code) => {
      this.child = null;
      for (const [, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error("worker exited"));
      }
      this.pending.clear();
      this.emit("event", { event: "worker.exit", code, message: `worker exited with code ${code}` } satisfies WorkerEvent);
    });
  }

  private onLine(line: string): void {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(line) as Record<string, unknown>;
    } catch {
      return;
    }
    if (typeof msg.event === "string") {
      this.emit("event", msg as unknown as WorkerEvent);
      return;
    }
    const id = msg.id;
    if (typeof id !== "number") return;
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    clearTimeout(p.timer);
    if (msg.error) {
      const e = msg.error as { message?: string };
      p.reject(new Error(e.message ?? "worker error"));
    } else {
      p.resolve(msg.result);
    }
  }

  call<T = unknown>(method: string, params: Record<string, unknown> = {}, timeoutMs = 30 * 60 * 1000): Promise<T> {
    if (!WORKER_METHODS.has(method)) return Promise.reject(new Error(`method not allowed: ${method}`));
    if (!this.child) this.start();
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} timed out`));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      this.child!.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    });
  }

  stop(): void {
    if (this.child) {
      this.child.stdin.write(JSON.stringify({ id: 0, method: "shutdown" }) + "\n");
      this.child.stdin.end();
    }
  }
}
