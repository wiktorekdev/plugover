// The Plugover host daemon's protocol: one JSON object per line, each way. On Linux it listens on
// a socket file; on Windows on a named pipe that only this plugin's AppContainer package (and
// LOCAL SERVICE) may open, since the container may not dial loopback or read the token file.

import { connect } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

const WINDOWS = process.platform === "win32";
/** Longest a request may take: a pairing or a slow attach is a few seconds. */
// Pairing a Bluetooth device through the relay can take most of a minute.
const TIMEOUT_MS = 75_000;
const SOCKET = WINDOWS
	? "\\\\.\\pipe\\plugover-host"
	: join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"), "plugover", "host.sock");

export type Reply = { ok: boolean; error?: string; [k: string]: unknown };

/** One request to the daemon: a JSON line out, a JSON line back. */
export function daemon(req: Record<string, unknown>): Promise<Reply> {
	return new Promise((resolve, reject) => {
		const s = connect({ path: SOCKET });
		let buf = "";
		let answered = false;
		s.setEncoding("utf8");
		// Pairing and plugging in wait on the network, but never this long; a page waiting forever
		// on a stuck daemon would look like it is still loading.
		s.setTimeout(TIMEOUT_MS, () => s.destroy(new Error("it did not answer in time")));
		s.on("connect", () => s.write(`${JSON.stringify(req)}\n`));
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
