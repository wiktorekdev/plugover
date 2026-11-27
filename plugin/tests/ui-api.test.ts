import { expect, mock, test } from "bun:test";

const forwarded: string[] = [];
mock.module("../src/daemon", () => ({
	daemon: async (body: { op: string }) => {
		forwarded.push(body.op);
		return { ok: true };
	},
}));

const { handle } = await import("../src/ui-api");

test("both desktop and Bluetooth controls reach the daemon", async () => {
	for (const op of ["desktop", "bt_scan", "bt_pair", "bt_forget", "device_name"]) {
		const response = await handle(new Request("http://localhost/api/" + op, { method: "POST" }));
		expect(await response?.json()).toEqual({ ok: true });
		expect(forwarded.at(-1)).toBe(op);
	}
});

test("only console operations go through the plugin", async () => {
	expect((await handle(new Request("http://localhost/api/session")))?.status).toBe(404);
	expect(await handle(new Request("http://localhost/index.html"))).toBeUndefined();
});
