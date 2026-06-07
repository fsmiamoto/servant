import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn } from "node:child_process";

type RunResult = {
  ok: boolean;
  exitCode: number;
  stdout: string;
  stderr: string;
  json: any | undefined;
  errorMessage: string | undefined;
};

function runServant(args: string[], signal?: AbortSignal): Promise<RunResult> {
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    let settled = false;

    const finish = (exitCode: number) => {
      if (settled) return;
      settled = true;
      let json: any | undefined;
      try {
        json = stdout.trim().length ? JSON.parse(stdout) : undefined;
      } catch {
        json = undefined;
      }
      let errorMessage: string | undefined;
      const trimmedErr = stderr.trim();
      if (trimmedErr.length) {
        try {
          const parsed = JSON.parse(trimmedErr);
          if (parsed && typeof parsed === "object") {
            errorMessage =
              parsed.error ?? parsed.message ?? JSON.stringify(parsed);
          } else {
            errorMessage = String(parsed);
          }
        } catch {
          errorMessage = trimmedErr;
        }
      }
      resolve({
        ok: exitCode === 0,
        exitCode,
        stdout,
        stderr,
        json,
        errorMessage,
      });
    };

    let child;
    try {
      child = spawn("servant", args, {
        env: { ...process.env, SERVANT_JSON: "1" },
        cwd: process.cwd(),
        signal,
      });
    } catch (err: any) {
      resolve({
        ok: false,
        exitCode: -1,
        stdout: "",
        stderr: String(err?.message ?? err),
        json: undefined,
        errorMessage: `Failed to spawn servant: ${err?.message ?? err}`,
      });
      return;
    }

    child.stdout?.on("data", (chunk) => {
      stdout += chunk.toString();
    });
    child.stderr?.on("data", (chunk) => {
      stderr += chunk.toString();
    });
    child.on("error", (err: any) => {
      if (settled) return;
      settled = true;
      resolve({
        ok: false,
        exitCode: -1,
        stdout,
        stderr: stderr + String(err?.message ?? err),
        json: undefined,
        errorMessage: `servant process error: ${err?.message ?? err}`,
      });
    });
    child.on("close", (code) => finish(code ?? -1));
  });
}

function friendlyError(res: RunResult): string {
  if (res.exitCode === 2) {
    return "servant daemon unreachable. Try `servant status` or `servant start`.";
  }
  if (res.exitCode === 1) {
    return res.errorMessage ?? "servant: user error";
  }
  const tail = (res.stderr.trim() || res.stdout.trim()).slice(0, 200);
  return `servant exited with code ${res.exitCode}${tail ? `: ${tail}` : ""}`;
}

function formatExpiry(secs: number | null | undefined): string {
  if (secs === null || secs === undefined) return "never";
  if (!Number.isFinite(secs) || secs <= 0) return "expired";
  const units: Array<[string, number]> = [
    ["d", 86400],
    ["h", 3600],
    ["m", 60],
    ["s", 1],
  ];
  const parts: string[] = [];
  let remaining = Math.floor(secs);
  for (const [label, size] of units) {
    const n = Math.floor(remaining / size);
    if (n > 0) {
      parts.push(`${n}${label}`);
      remaining -= n * size;
    }
    if (parts.length === 2) break;
  }
  return parts.length ? parts.join(" ") : "0s";
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return "…" + s.slice(s.length - max + 1);
}

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "servant_serve",
    label: "Serve a file or folder over HTTP",
    description:
      "Register a local file or directory with the servant daemon and return a shareable URL. Re-serving the same path slides the TTL.",
    parameters: Type.Object({
      path: Type.String({
        description:
          "File or directory to serve. Absolute or relative to the current working directory.",
      }),
      ttl: Type.Optional(
        Type.String({
          description:
            "Lifetime like '30s','5m','2h','24h','7d', or 'never'. Default: 24h sliding.",
        }),
      ),
      name: Type.Optional(
        Type.String({ description: "Override the URL slug." }),
      ),
    }),
    async execute(_id, params, signal) {
      const argv = ["serve", params.path];
      if (params.ttl) argv.push("--ttl", params.ttl);
      if (params.name) argv.push("--name", params.name);
      const res = await runServant(argv, signal);
      if (!res.ok || !res.json) {
        return {
          content: [{ type: "text", text: friendlyError(res) }],
          details: { exitCode: res.exitCode, stderr: res.stderr },
          isError: true,
        };
      }
      const r = res.json;
      const text =
        `${r.reused ? "Reused" : "Registered"} ${r.kind} → ${r.url}\n` +
        `  id: ${r.id}\n  source: ${r.source_path}\n` +
        `  expires: ${formatExpiry(r.expires_in_secs)}`;
      return {
        content: [{ type: "text", text }],
        details: r,
      };
    },
  });

  pi.registerTool({
    name: "servant_ls",
    label: "List active servant registrations",
    description:
      "List all files and folders currently served by the servant daemon.",
    parameters: Type.Object({}),
    async execute(_id, _params, signal) {
      const res = await runServant(["ls"], signal);
      if (!res.ok) {
        return {
          content: [{ type: "text", text: friendlyError(res) }],
          details: { exitCode: res.exitCode, stderr: res.stderr },
          isError: true,
        };
      }
      const rows: any[] = Array.isArray(res.json) ? res.json : [];
      if (rows.length === 0) {
        return {
          content: [
            { type: "text", text: "(no active servant registrations)" },
          ],
          details: { count: 0, rows: [] },
        };
      }
      const header = "id    expires        url-path                            source";
      const lines = [header];
      for (const r of rows) {
        const id = String(r.id ?? "").padEnd(5).slice(0, 5);
        const expires = formatExpiry(r.expires_in_secs).padEnd(14).slice(0, 14);
        const urlPath = String(r.url_path ?? "").padEnd(36).slice(0, 36);
        const missing = r.missing ? " (missing)" : "";
        const source = truncate(String(r.source_path ?? ""), 60);
        lines.push(`${id} ${expires} ${urlPath} ${source}${missing}`);
      }
      return {
        content: [{ type: "text", text: lines.join("\n") }],
        details: { count: rows.length, rows },
      };
    },
  });

  pi.registerTool({
    name: "servant_rm",
    label: "Remove a servant registration",
    description:
      "Unregister a served file or folder. Target can be a numeric id, a URL path like '/foo.html', a full URL, or the absolute source path.",
    parameters: Type.Object({
      target: Type.String({
        description:
          "Numeric id, URL path like '/foo.html', full URL, or absolute source path.",
      }),
    }),
    async execute(_id, params, signal) {
      const res = await runServant(["rm", params.target], signal);
      if (!res.ok) {
        return {
          content: [{ type: "text", text: friendlyError(res) }],
          details: { exitCode: res.exitCode, stderr: res.stderr },
          isError: true,
        };
      }
      const removed = res.json?.removed;
      const text = removed
        ? `Removed ${removed.kind} ${removed.url_path} (id ${removed.id})\n  source: ${removed.source_path}`
        : "Removed.";
      return {
        content: [{ type: "text", text }],
        details: res.json,
      };
    },
  });
}
