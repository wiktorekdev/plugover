import operations from "../../data/ui-operations.txt" with { type: "text" };
import { daemon } from "./daemon";

const UI_OPS = new Set(operations.trim().split(/\s+/));

const json = (body: unknown, status = 200) =>
	new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

export async function handle(req: Request): Promise<Response | undefined> {
	const { pathname } = new URL(req.url);
	if (!pathname.startsWith("/api/")) return undefined;
	const op = pathname.slice("/api/".length);
	if (!UI_OPS.has(op)) return json({ ok: false, error: "unknown action" }, 404);
	const body = req.method === "POST" ? ((await req.json().catch(() => ({}))) as Record<string, unknown>) : {};
	try {
		return json(await daemon({ ...body, op }));
	} catch (e) {
		return json({ ok: false, offline: true, error: (e as Error).message }, 503);
	}
}
