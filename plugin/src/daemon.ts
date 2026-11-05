// The Plugover host daemon's protocol: one JSON object per line, each way. On Linux it listens on
// a socket file; on Windows on loopback TCP, where every request carries the token it writes to
// %ProgramData%\Plugover\api\api-token on start.

import { readFileSync } from "node:fs";
import { type NetConnectOpts, connect } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

const WINDOWS = process.platform === "win32";
const WINDOWS_PORT = 7276;
/** Longest a request may take: a pairing or a slow attach is a few seconds. */
const TIMEOUT_MS = 30_000;
const PROGRAM_DATA = join(process.env.ProgramData ?? "C:\\ProgramData", "Plugover");
const SOCKET = join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"), "plugover", "host.sock");

export type Reply = { ok: boolean; error?: string; [k: string]: unknown };

function target(): { opts: NetConnectOpts; token?: string } {
	if (!WINDOWS) return { opts: { path: SOCKET } };
	// Read on every request: the daemon picks a new token each time it starts.
	const token = readFileSync(join(PROGRAM_DATA, "api", "api-token"), "utf8").trim();
	return { opts: { host: "127.0.0.1", port: WINDOWS_PORT }, token };
}

/** One request to the daemon: a JSON line out, a JSON line back. */
export function daemon(req: Record<string, unknown>): Promise<Reply> {
	return new Promise((resolve, reject) => {
		let t: ReturnType<typeof target>;
		try {
			t = target();
		} catch (e) {
			reject(new Error(`the Plugover daemon is not running (${(e as Error).message})`));
			return;
		}
		const s = connect(t.opts);
		let buf = "";
		let answered = false;
		s.setEncoding("utf8");
		// Pairing and plugging in wait on the network, but never this long; a page waiting forever
		// on a stuck daemon would look like it is still loading.
		s.setTimeout(TIMEOUT_MS, () => s.destroy(new Error("it did not answer in time")));
		s.on("connect", () => s.write(`${JSON.stringify(t.token ? { ...req, token: t.token } : req)}\n`));
		s.on("data", (chunk) => {
			buf += chunk;
			const end = buf.indexOf("\n");
			if (end < 0) return;
			answered = true;
			s.end();
			try {
				resolve(JSON.parse(buf.slice(0, end)) as Reply);
			} catch (e) {
				reject(e);
			}
		});
		s.on("error", (e) => reject(new Error(`the Plugover daemon is not running (${e.message})`)));
		s.on("close", () => answered || reject(new Error("the Plugover daemon hung up without answering")));
	});
}
