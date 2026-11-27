import { expect, test } from "bun:test";
import { latestRefresh } from "../src/latest-refresh";

type StatusResult = { status: string } | { error: string };

function pendingRefreshes() {
	const finish: ((result: StatusResult) => void)[] = [];
	const applied: StatusResult[] = [];
	const { refresh } = latestRefresh(
		() => new Promise<StatusResult>((resolve) => finish.push(resolve)),
		(result) => applied.push(result),
	);
	return { applied, finish, refresh };
}

test("a slow poll cannot replace the status refreshed after an action", async () => {
	const { applied, finish, refresh } = pendingRefreshes();
	const poll = refresh();
	const action = refresh();
	finish[1]({ status: "paired" });
	await action;
	finish[0]({ status: "no relays" });
	await poll;
	expect(applied).toEqual([{ status: "paired" }]);
});

test("an older failure cannot displace status while its replacement is pending", async () => {
	const { applied, finish, refresh } = pendingRefreshes();
	const poll = refresh();
	const action = refresh();
	finish[0]({ error: "daemon unavailable" });
	await poll;
	expect(applied).toEqual([]);
	finish[1]({ status: "paired" });
	await action;
	expect(applied).toEqual([{ status: "paired" }]);
});
