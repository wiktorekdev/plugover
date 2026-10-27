// Bridges punktfunk and the Plugover host daemon: tells the daemon when a stream starts and
// stops, and serves the settings page inside the punktfunk console. All USB work happens in
// the daemon, which runs outside the plugin sandbox.

import { type Punktfunk, definePlugin, servePluginUi } from "@punktfunk/host";
import { daemon } from "./daemon";

const UI_OPS = new Set(["status", "discover", "pair", "forget", "auto", "attach", "detach"]);
const RESYNC_MS = 10_000;

const json = (body: unknown, status = 200) =>
	new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

async function handle(req: Request): Promise<Response | undefined> {
	const { pathname } = new URL(req.url);
	const op = pathname.match(/^\/api\/([a-z]+)$/)?.[1];
	if (!op) return undefined;
	if (!UI_OPS.has(op)) return json({ ok: false, error: "unknown action" }, 404);
	const body = req.method === "POST" ? ((await req.json().catch(() => ({}))) as Record<string, unknown>) : {};
	try {
		return json(await daemon({ ...body, op }));
	} catch (e) {
		return json({ ok: false, offline: true, error: (e as Error).message }, 503);
	}
}

/** Keeps the daemon's idea of "a stream is running" in step with punktfunk. */
function followStreams(pf: Punktfunk) {
	let told: boolean | undefined;
	const tell = async (active: boolean) => {
		if (told === active) return;
		try {
			await daemon({ op: "session", active });
			told = active;
		} catch (e) {
			console.warn(`plugover: ${(e as Error).message}`);
		}
	};
	const resync = async () => {
		try {
			const status = (await pf.request("GET", "/status")) as { active_sessions?: number };
			await tell((status.active_sessions ?? 0) > 0);
		} catch (e) {
			console.warn(`plugover: could not read the stream state (${(e as Error).message})`);
		}
	};
	pf.events.on("stream.started", () => tell(true));
	pf.events.on("stream.stopped", resync);
	pf.events.on("session.ended", resync);
	// Covers a missed event and a daemon that restarted mid-stream.
	setInterval(() => {
		told = undefined;
		void resync();
	}, RESYNC_MS);
	void resync();
}

export default definePlugin({
	name: "plugover",
	main: async (pf) => {
		followStreams(pf);
		await servePluginUi(pf, {
			id: "plugover",
			title: "USB devices",
			icon: "cable",
			version: "0.4.0",
			staticDir: new URL("./ui", import.meta.url),
			fetch: handle,
		});
		await new Promise(() => {});
	},
});
