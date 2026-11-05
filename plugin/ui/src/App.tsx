import { Badge } from "@unom/ui/badge";
import { Button } from "@unom/ui/button";
import Card from "@unom/ui/card";
import { EmptyState } from "@unom/ui/empty-state";
import { InputText } from "@unom/ui/form/input-text";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@unom/ui/form/select";
import { Switch } from "@unom/ui/form/switch";
import { cn } from "@unom/ui/lib/utils";
import { Spinner } from "@unom/ui/spinner";
import { Toaster, toast } from "@unom/ui/toast";
import { ChevronDown, ChevronLeft, Ellipsis, Monitor, Pause, Play, Plus, Unplug } from "lucide-react";
import { type FormEvent, useCallback, useEffect, useState } from "react";
import {
	ApiError,
	call,
	type Device,
	type Found,
	type Kind,
	KIND_LABEL,
	type Latency,
	type Missing,
	newer,
	type Relay,
	SOURCE_LABEL,
	type Status,
	steady,
} from "./api";
import { KIND_ICON } from "./icons";

const POLL_MS = 2000;
const EMBEDDED = window.self !== window.top;
const TOAST_COLORS = {
	"--normal-bg": "var(--color-neutral)",
	"--normal-border": "var(--color-border)",
	"--normal-text": "var(--color-foreground)",
} as React.CSSProperties;
/** Kinds in the order they are declared, game devices first. */
const KIND_ORDER = Object.keys(KIND_LABEL) as Kind[];
/** An off switch is the card's own color; the outline keeps it visible. */
const OFF_RING = "data-[state=unchecked]:ring-1 data-[state=unchecked]:ring-muted-foreground/40";

export function App() {
	const [status, setStatus] = useState<Status>();
	const [error, setError] = useState<ApiError>();
	const [adding, setAdding] = useState(false);

	const refresh = useCallback(async () => {
		try {
			setStatus(await call<Status>("status"));
			setError(undefined);
		} catch (e) {
			setError(e instanceof ApiError ? e : new ApiError(String(e), false));
		}
	}, []);

	// The next poll waits for the last answer, so a slow daemon is not sent a pile of requests.
	useEffect(() => {
		let stopped = false;
		let timer: ReturnType<typeof setTimeout>;
		const poll = async () => {
			await refresh();
			if (!stopped) timer = setTimeout(poll, POLL_MS);
		};
		void poll();
		return () => {
			stopped = true;
			clearTimeout(timer);
		};
	}, [refresh]);

	let content: React.ReactNode;
	if (error?.offline) {
		content = (
			<EmptyState
				icon={Unplug}
				variant="error"
				title="Plugover host daemon is not running"
				description="Run the Plugover installer on this computer and pick Host. If it is installed already, plugover doctor says why it is not running."
			/>
		);
	} else if (error && !status) {
		content = <EmptyState icon={Unplug} variant="error" title="Could not load devices" description={error.message} />;
	} else if (!status) {
		content = (
			<div className="flex justify-center py-24">
				<Spinner />
			</div>
		);
	} else if (status.relays.length === 0 || adding) {
		const paired = () => {
			setAdding(false);
			void refresh();
		};
		content = <Setup onPaired={paired} onCancel={status.relays.length > 0 ? () => setAdding(false) : undefined} />;
	} else {
		content = (
			<>
				<StreamBanner status={status} />
				{status.relays.map((r) => (
					<RelayView key={r.id} relay={r} hostVersion={status.version} session={status.session} refresh={refresh} />
				))}
				{/* Close under the last relay rather than a whole section gap away. */}
				<div className="-mt-7">
					<Button size="sm" variant="ghost" onClick={() => setAdding(true)}>
						<Plus className="size-4" /> Pair another relay
					</Button>
				</div>
			</>
		);
	}

	return (
		<main className="mx-auto flex max-w-3xl flex-col gap-10 px-4 py-8 sm:px-6">
			{/* Inside the console its page header already says Plugover. */}
			{!EMBEDDED && <h1 className="-mb-4 text-2xl font-semibold tracking-tight">Plugover</h1>}
			{content}
			{/* Sonner defaults to light; the console and this page are always dark, in the theme's colors. */}
			<Toaster theme="dark" style={TOAST_COLORS} />
		</main>
	);
}

/** What a stream does right now, the first thing to know on this page. */
function StreamBanner({ status }: { status: Status }) {
	const here = status.relays.flatMap((r) => r.devices.filter((d) => d.state === "here").map(() => r.name));
	const failing = status.relays.flatMap((r) => r.devices.filter((d) => d.error)).length;
	const from = [...new Set(here)];
	const count = (n: number) => `${n} ${n === 1 ? "device" : "devices"}`;
	const devices = count(here.length);
	const Icon = status.session ? Play : Pause;
	return (
		<Card muted colorVariant={status.session ? "accent" : "default"} className="flex-row items-center gap-4">
			<Icon className={cn("size-5 shrink-0", status.session ? "text-primary" : "text-muted-foreground")} />
			<div className="min-w-0 flex-1">
				<p className="font-medium">{status.session ? "Stream running" : "No stream"}</p>
				<p className="text-sm text-muted-foreground">
					{status.session
						? here.length > 0
							? `${devices} connected from ${from.join(", ")}`
							: "No devices connected"
						: here.length > 0
							? `${devices} connected outside a stream`
							: "Devices you turn on connect when a stream starts."}
				</p>
				{failing > 0 && <p className="text-sm text-destructive">{count(failing)} could not connect; see below.</p>}
			</div>
			<p className="hidden text-sm text-muted-foreground sm:block">
				{status.host}
				{status.version && ` · ${status.version}`}
			</p>
		</Card>
	);
}

function Setup({ onPaired, onCancel }: { onPaired: () => void; onCancel?: () => void }) {
	const [found, setFound] = useState<Found[]>();
	const [target, setTarget] = useState<{ name: string; address: string }>();
	const [address, setAddress] = useState("");

	const discover = useCallback(async () => {
		setFound(undefined);
		try {
			setFound((await call<{ relays: Found[] }>("discover", {})).relays);
		} catch (e) {
			setFound([]);
			toast.error((e as Error).message);
		}
	}, []);

	useEffect(() => {
		void discover();
	}, [discover]);

	const pair = (name: string, address: string) => setTarget({ name, address });

	if (target) return <CodeEntry {...target} onCancel={() => setTarget(undefined)} onPaired={onPaired} />;

	const submit = (e: FormEvent) => {
		e.preventDefault();
		if (address.trim()) void pair(address.trim(), address.trim());
	};

	return (
		<section className="flex flex-col gap-6">
			{onCancel && <BackLink onClick={onCancel} />}
			<header>
				<h1 className="text-2xl font-semibold tracking-tight">Pair a relay</h1>
				<p className="mt-1 max-w-prose text-muted-foreground">
					The relay is the machine your devices are plugged into.
				</p>
			</header>

			<Card padding={false}>
				{found === undefined ? (
					<div className="flex items-center gap-3 p-5 text-muted-foreground">
						<Spinner /> Searching the local network
					</div>
				) : found.length === 0 ? (
					<p className="p-5 text-muted-foreground">
						No relays found. Check that the relay is running and its firewall allows TCP 7275 and UDP 5353.
					</p>
				) : (
					<ul>
						{found.map((f) => {
							// Older daemons only list addresses; the first is the best guess there.
							const address = f.address === undefined ? f.addresses[0] : f.address;
							return (
								<li key={f.id} className="flex items-center gap-4 border-b border-border p-4 last:border-0">
									<Monitor className="size-5 text-primary" />
									<div className="min-w-0 flex-1">
										<p className="font-medium">{f.name}</p>
										<p className="truncate text-sm text-muted-foreground">
											{address ?? `Not reachable from this computer (${f.addresses.join(", ") || "no address"})`}
										</p>
									</div>
									{f.paired ? (
										<Badge variant="success">Paired</Badge>
									) : (
										<Button disabled={!address} onClick={() => address && pair(f.name, address)}>
											Pair
										</Button>
									)}
								</li>
							);
						})}
					</ul>
				)}
			</Card>

			<div className="flex flex-wrap items-end justify-between gap-4">
				<form onSubmit={submit} className="flex flex-wrap items-end gap-2">
					<label className="flex flex-col gap-1 text-sm">
						<span className="text-muted-foreground">Relay address</span>
						<InputText value={address} onChange={(e) => setAddress(e.target.value)} placeholder="192.168.1.50" />
					</label>
					<Button type="submit" variant="secondary" className="h-input-height px-6">
						Pair
					</Button>
				</form>
				<Button variant="ghost" className="h-input-height" onClick={discover}>
					Search again
				</Button>
			</div>
		</section>
	);
}

function CodeEntry({
	name,
	address,
	onCancel,
	onPaired,
}: { name: string; address: string; onCancel: () => void; onPaired: () => void }) {
	const [code, setCode] = useState("");
	const [busy, setBusy] = useState(false);
	const [failure, setFailure] = useState<string>();
	const digits = code.replace(/\D/g, "");

	const submit = async (e: FormEvent) => {
		e.preventDefault();
		setBusy(true);
		setFailure(undefined);
		try {
			await call("pair", { address, code: digits });
			toast.success(`Paired with ${name}`);
			onPaired();
		} catch (err) {
			setFailure((err as Error).message);
		} finally {
			setBusy(false);
		}
	};

	return (
		<section className="flex flex-col gap-6">
			<BackLink onClick={onCancel} />
			<header>
				<h1 className="text-2xl font-semibold tracking-tight">Pair with {name}</h1>
				<p className="mt-1 max-w-prose text-muted-foreground">
					Enter the six-digit code from the relay. Its install prints one at the end.
				</p>
			</header>
			<Card className="gap-4">
				<form onSubmit={submit} className="flex flex-wrap items-center gap-3">
					<InputText
						value={code}
						onChange={(e) => setCode(e.target.value)}
						placeholder="000 000"
						inputMode="numeric"
						autoComplete="one-time-code"
						aria-label="Pairing code"
						autoFocus
						className="w-44 text-center font-mono text-xl tracking-[0.3em] md:text-xl"
					/>
					<Button type="submit" className="h-input-height px-6" disabled={busy || digits.length !== 6}>
						{busy ? <Spinner /> : "Pair"}
					</Button>
				</form>
				{failure && <p className="text-sm text-destructive">Pairing failed: {failure}</p>}
				<p className="text-sm text-muted-foreground">
					Lost it? Run <Cmd>plugover code</Cmd> on {name} as root or administrator for a new one. A code works once,
					for 15 minutes.
				</p>
			</Card>
		</section>
	);
}

function BackLink({ onClick }: { onClick: () => void }) {
	return (
		<div className="-mb-3">
			<Button size="sm" variant="ghost" className="-ml-3" onClick={onClick}>
				<ChevronLeft className="size-4" /> Back
			</Button>
		</div>
	);
}

async function act(op: string, body: object, done: string, refresh: () => Promise<void>) {
	try {
		await call(op, body);
		toast.success(done);
		await refresh();
	} catch (e) {
		toast.error((e as Error).message);
	}
}

function RelayView({
	relay,
	hostVersion,
	session,
	refresh,
}: { relay: Relay; hostVersion?: string; session: boolean; refresh: () => Promise<void> }) {
	// Hubs, storage, network adapters and what the relay does not share are left out. Game devices
	// come first, in a fixed order, so rows do not jump around when they are turned on or off.
	const devices = relay.devices.filter((d) => !d.blocked).sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || a.title.localeCompare(b.title));
	const missing = relay.missing ?? [];
	const unpaired = relay.error?.includes("pair again") ?? false;
	// A relay too old to encrypt gets its own card, which says to update it too.
	const mismatch = relay.online && relay.encrypted && hostVersion && relay.version !== hostVersion;
	// Which side to update: the relay before 0.5 does not say its version, so it is the old one.
	const relayOlder = !relay.version || (hostVersion !== undefined && newer(hostVersion, relay.version));
	const slow = relay.online && relay.latency && !steady(relay.latency);
	const [repairing, setRepairing] = useState(false);
	const [menu, setMenu] = useState(false);
	// Several of the same model: rows say which port is which.
	const twins = (d: Device) => relay.devices.filter((o) => o.id === d.id).length > 1;

	// Pairing again keeps this relay's auto-connect choices; forgetting it would not.
	if (repairing) {
		const done = () => {
			setRepairing(false);
			void refresh();
		};
		return <CodeEntry name={relay.name} address={relay.address} onCancel={() => setRepairing(false)} onPaired={done} />;
	}

	return (
		<section className="flex flex-col gap-4">
			<div className="flex flex-col gap-3">
				<header className="flex items-center gap-3">
					<span
						className={cn("size-2.5 shrink-0 rounded-full", relay.online ? "bg-success" : unpaired ? "bg-destructive" : "bg-warn")}
						aria-hidden
					/>
					<div className="min-w-0 flex-1">
						<h2 className="flex items-center gap-2 text-lg font-medium">
							<span className="truncate">{relay.name}</span>
							{relay.unencrypted && (
								<Badge size="sm" variant="warn">
									Not encrypted
								</Badge>
							)}
						</h2>
						<p className="truncate text-sm text-muted-foreground">
							{relay.online ? "Online" : unpaired ? "Unpaired" : "Offline"}
							{relay.online && relay.latency && (
								<span className={cn(slow && "text-warn")} title="Round trip to the relay over the last few seconds">
									{` · ${ms(relay.latency.average)}`}
								</span>
							)}
							{` · ${relay.address}`}
							{relay.version && ` · Plugover ${relay.version}`}
							{relay.encrypted && " · Encrypted"}
						</p>
					</div>
					<Button size="icon" variant="ghost" aria-label={`Options for ${relay.name}`} aria-expanded={menu} onClick={() => setMenu((m) => !m)}>
						<Ellipsis className="size-4" />
					</Button>
				</header>
				{menu && (
					<div
						className="flex flex-wrap items-center justify-end gap-2 text-sm text-muted-foreground"
						onKeyDown={(e) => e.key === "Escape" && setMenu(false)}
					>
						<Button size="sm" variant="ghost" onClick={() => setRepairing(true)}>
							Pair again
						</Button>
						<ForgetButton relay={relay} refresh={refresh} />
					</div>
				)}
			</div>

			{!relay.online && (
				<Card muted>
					<p className="font-medium">
						{unpaired ? `${relay.name} no longer knows this computer.` : `${relay.name} is not responding at ${relay.address}.`}
					</p>
					{relay.error && !unpaired && <p className="mt-1 break-words text-sm text-muted-foreground">{relay.error}</p>}
					<p className="mt-3 max-w-prose text-sm text-muted-foreground">
						{unpaired ? (
							"It was unpaired on the relay. Pair again with a new code to get its devices back."
						) : (
							<>
								Check that it is on, that the relay service runs and that its firewall allows TCP 7275 from this
								computer. <Cmd>plugover doctor</Cmd> on either machine says what is wrong.
							</>
						)}
					</p>
					{unpaired && (
						<Button className="mt-4" size="sm" onClick={() => setRepairing(true)}>
							Pair again
						</Button>
					)}
				</Card>
			)}

			{mismatch && (
				<Card muted className="border border-warn/50">
					<p className="text-sm">
						{relay.name} runs Plugover {relay.version ?? "older than 0.5"} and this computer {hostVersion}.{" "}
						{relayOlder ? (
							relay.version ? (
								<>
									Run <Cmd>plugover update</Cmd> on {relay.name}.
								</>
							) : (
								`Run the installer on ${relay.name} again.`
							)
						) : (
							<>
								Run <Cmd>plugover update</Cmd> on this computer.
							</>
						)}
					</p>
				</Card>
			)}

			{relay.online && !relay.encrypted && <Unencrypted relay={relay} refresh={refresh} />}

			{slow && relay.latency && <SlowLink relay={relay.name} latency={relay.latency} />}

			<div className="flex flex-col gap-2">
				<Card padding={false}>
					{devices.map((d) => (
						<DeviceRow key={d.busid} relay={relay} device={d} twins={twins(d)} session={session} refresh={refresh} />
					))}
					{missing.map((m) => (
						<MissingRow key={`${m.id}-${m.port ?? ""}`} relay={relay} device={m} refresh={refresh} />
					))}
					{devices.length + missing.length === 0 && (
						<p className="p-5 text-muted-foreground">
							{relay.online ? `Nothing plugged into ${relay.name} can be shared.` : "Devices show up here once the relay answers."}
						</p>
					)}
				</Card>
			</div>
		</section>
	);
}

/** A relay too old to encrypt: its devices stay off until it is updated, or you allow it. */
function Unencrypted({ relay, refresh }: { relay: Relay; refresh: () => Promise<void> }) {
	const [confirming, setConfirming] = useState(false);
	const [busy, setBusy] = useState(false);
	const set = async (on: boolean) => {
		setBusy(true);
		await act(
			"unencrypted",
			{ relay: relay.id, on },
			on ? `${relay.name} connects without encryption` : `${relay.name} connects only encrypted`,
			refresh,
		);
		setBusy(false);
		setConfirming(false);
	};
	const version = relay.version ?? "older than 0.5";
	const risk = "anyone on your network could watch these devices or fake their input, such as keystrokes";
	if (relay.unencrypted) {
		return (
			<Card muted className="border border-warn/50">
				<p className="text-sm">
					{relay.name} runs Plugover {version}, which cannot encrypt, and you allowed it to connect anyway: {risk}. Update
					Plugover on {relay.name} to encrypt again.
				</p>
				<Button className="mt-3 self-start" size="sm" variant="secondary" disabled={busy} onClick={() => set(false)}>
					Stop connecting without encryption
				</Button>
			</Card>
		);
	}
	return (
		<Card muted className="border border-destructive/50">
			<p className="font-medium">{relay.name} cannot encrypt, so its devices do not connect.</p>
			<p className="mt-1 max-w-prose text-sm text-muted-foreground">
				It runs Plugover {version}. Run <Cmd>plugover update</Cmd> on {relay.name}, or connect without encryption until you
				do.
			</p>
			{confirming ? (
				<div role="alert" className="mt-3 flex flex-col gap-3">
					<p className="max-w-prose text-sm">Without encryption, {risk}. Do this only on a network you trust.</p>
					<div className="flex flex-wrap gap-2">
						<Button size="sm" variant="destructive" disabled={busy} onClick={() => set(true)}>
							Connect without encryption
						</Button>
						<Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
							Cancel
						</Button>
					</div>
				</div>
			) : (
				<Button className="mt-3 self-start" size="sm" variant="secondary" onClick={() => setConfirming(true)}>
					Connect without encryption…
				</Button>
			)}
		</Card>
	);
}

/** Force feedback needs a quick and steady link, and a wobbly one is the usual cause of trouble. */
function SlowLink({ relay, latency }: { relay: string; latency: Latency }) {
	return (
		<Card muted className="border border-warn/50">
			<p className="text-sm">
				The network to {relay} is slow or uneven: {ms(latency.average)} on average, varying by {ms(latency.jitter)}. Force
				feedback needs a steady link; connecting both computers by cable usually fixes it.
			</p>
		</Card>
	);
}

/** Unix seconds as a time of day, with the date when it was not today. */
function clock(unix: number) {
	const at = new Date(unix * 1000);
	const time = at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
	return at.toDateString() === new Date().toDateString() ? time : `${at.toLocaleDateString()} ${time}`;
}

function ms(n: number) {
	return `${n < 10 ? n.toFixed(1) : Math.round(n)} ms`;
}

function Cmd({ children }: { children: React.ReactNode }) {
	return <code className="whitespace-nowrap rounded bg-muted px-1.5 py-0.5">{children}</code>;
}

/** Without a device: one that is chosen but not plugged in. */
function KindTile({ device }: { device?: Device }) {
	const Icon = (device && KIND_ICON[device.kind]) ?? KIND_ICON.other;
	return (
		<div className={cn("grid size-11 shrink-0 place-items-center rounded-xl bg-neutral-accent text-primary", !device && "opacity-50")}>
			<Icon className="size-6" />
		</div>
	);
}

function DeviceRow({
	relay,
	device: d,
	twins,
	session,
	refresh,
}: { relay: Relay; device: Device; twins: boolean; session: boolean; refresh: () => Promise<void> }) {
	const [open, setOpen] = useState(false);
	/** What the switch is doing right now, which can take a few seconds during a stream. */
	const [busy, setBusy] = useState<string>();
	const here = d.state === "here";
	const run = async (op: string, body: object, done: string, doing: string) => {
		setBusy(doing);
		await act(op, { relay: relay.id, ...body }, done, refresh);
		setBusy(undefined);
	};
	const note =
		busy ??
		(d.state === "elsewhere" ? `In use by ${d.lent_to}` : d.auto && !here && !session ? "Connects when a stream starts" : undefined);

	return (
		<div className="border-b border-border last:border-0">
			<div className="flex items-center gap-4 p-4">
				<KindTile device={d} />
				<div className="min-w-0 flex-1">
					<p className="font-medium [overflow-wrap:anywhere]">{d.title}</p>
					{/* Wraps rather than cuts off on a phone: the note is what the row is there to say. */}
					<div className="mt-0.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm text-muted-foreground">
						<span>
							{KIND_LABEL[d.kind]}
							{(twins || d.auto_port) && ` · port ${d.busid}${d.auto_port ? " only" : ""}`}
							{note && ` · ${note}`}
						</span>
						{here && <Badge size="sm">Connected</Badge>}
					</div>
					{d.error && !busy && <p className="mt-1 break-words text-sm text-destructive">Could not connect: {d.error}</p>}
				</div>
				<Switch
					className={OFF_RING}
					checked={d.auto}
					disabled={busy !== undefined}
					aria-label={`Use ${d.title} here during streams`}
					onCheckedChange={(on) =>
						run(
							"auto",
							{ device: d.id, on },
							on ? (session ? `${d.title} connected` : `${d.title} connects when a stream starts`) : `${d.title} is free again`,
							on ? (session ? "Connecting…" : "Saving…") : here ? "Releasing…" : "Saving…",
						)
					}
				/>
				<Button size="icon" variant="ghost" aria-label={`Details for ${d.title}`} aria-expanded={open} onClick={() => setOpen((o) => !o)}>
					<ChevronDown className={cn("size-4 transition-transform", open && "rotate-180")} />
				</Button>
			</div>
			{open && (
				<dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-1 px-4 pb-4 text-sm sm:pl-19">
					<dt className="text-muted-foreground">Maker</dt>
					<dd>{d.maker}</dd>
					<dt className="text-muted-foreground">USB ID</dt>
					<dd className="font-mono">{d.id}</dd>
					<dt className="text-muted-foreground">Port</dt>
					<dd className="font-mono">{d.busid}</dd>
					<dt className="text-muted-foreground">Name from</dt>
					<dd>{SOURCE_LABEL[d.source]}</dd>
					{d.history.drops > 0 && (
						<>
							<dt className="text-muted-foreground">Dropped out</dt>
							<dd>
								{d.history.drops === 1 ? "Once" : `${d.history.drops} times`}
								{d.history.last_drop && `, last at ${clock(d.history.last_drop)}`}
							</dd>
						</>
					)}
					{d.history.last_error && (
						<>
							<dt className="text-muted-foreground">Last error</dt>
							<dd className="break-words">
								{d.history.last_error_at && `${clock(d.history.last_error_at)}: `}
								{d.history.last_error}
							</dd>
						</>
					)}
					{d.auto && (twins || d.auto_port) && (
						<>
							<dt className="text-muted-foreground">Only this port</dt>
							<dd>
								<Switch
									className={OFF_RING}
									checked={d.auto_port}
									disabled={busy !== undefined}
									aria-label={`Auto-connect only the ${d.title} on port ${d.busid}`}
									onCheckedChange={(only) =>
										run(
											"auto",
											{ device: d.id, on: true, port: only ? d.busid : undefined },
											only ? `Only the ${d.title} on port ${d.busid} connects` : `Any ${d.title} connects`,
											"Saving…",
										)
									}
								/>
							</dd>
						</>
					)}
					<dt className="pt-1.5 text-muted-foreground">Wrong name or kind?</dt>
					<dd>
						<LabelForm device={d} refresh={refresh} />
					</dd>
				</dl>
			)}
		</div>
	);
}

/** Names a model or corrects its kind on this host; every device with the same USB id follows. */
function LabelForm({ device: d, refresh }: { device: Device; refresh: () => Promise<void> }) {
	const [name, setName] = useState(d.source === "user" ? d.title : "");
	const [kind, setKind] = useState<Kind>(d.kind);
	const [busy, setBusy] = useState(false);
	const save = async (body: object, done: string) => {
		setBusy(true);
		await act("label", { device: d.id, ...body }, done, refresh);
		setBusy(false);
	};
	const submit = (e: FormEvent) => {
		e.preventDefault();
		save({ name: name.trim() || undefined, kind }, `${name.trim() || d.title} saved`);
	};

	return (
		<form onSubmit={submit} className="flex flex-wrap items-center gap-2">
			<InputText
				value={name}
				onChange={(e) => setName(e.target.value)}
				placeholder={d.title}
				maxLength={60}
				aria-label={`Name for ${d.title}`}
				className="w-56"
			/>
			<Select value={kind} onValueChange={(k) => setKind(k as Kind)}>
				<SelectTrigger size="sm" aria-label={`Kind of ${d.title}`} className="w-44">
					<SelectValue />
				</SelectTrigger>
				<SelectContent>
					{(Object.keys(KIND_LABEL) as Kind[]).map((k) => (
						<SelectItem key={k} value={k}>
							{KIND_LABEL[k]}
						</SelectItem>
					))}
				</SelectContent>
			</Select>
			<Button type="submit" size="sm" variant="secondary" disabled={busy}>
				Save
			</Button>
			{d.source === "user" && (
				<Button type="button" size="sm" variant="ghost" disabled={busy} onClick={() => save({}, "Back to what Plugover detects")}>
					Reset
				</Button>
			)}
		</form>
	);
}

function MissingRow({ relay, device: m, refresh }: { relay: Relay; device: Missing; refresh: () => Promise<void> }) {
	const [busy, setBusy] = useState(false);
	const off = async () => {
		setBusy(true);
		await act("auto", { relay: relay.id, device: m.id, on: false, port: m.port ?? undefined }, `${m.title} removed`, refresh);
		setBusy(false);
	};
	return (
		<div className="flex items-center gap-4 border-b border-border p-4 last:border-0">
			<KindTile />
			<div className="min-w-0 flex-1">
				<p className="font-medium text-muted-foreground [overflow-wrap:anywhere]">{m.title}</p>
				<p className="mt-0.5 text-sm text-muted-foreground">
					{relay.online ? `Not plugged into ${relay.name}${m.port ? ` at port ${m.port}` : ""}` : "Waiting for the relay"}
				</p>
			</div>
			<Switch checked disabled={busy} aria-label={`Use ${m.title} here during streams`} onCheckedChange={off} />
			{/* Where the details button sits in the other rows. */}
			<div className="size-9 shrink-0" />
		</div>
	);
}

function ForgetButton({ relay, refresh }: { relay: Relay; refresh: () => Promise<void> }) {
	const [confirming, setConfirming] = useState(false);
	if (!confirming) {
		return (
			<Button size="sm" variant="ghost" onClick={() => setConfirming(true)}>
				Forget {relay.name}
			</Button>
		);
	}
	return (
		<span role="alert" className="flex flex-wrap items-center justify-end gap-2">
			Forget {relay.name}? Its devices and your choices for them go until you pair again.
			<Button size="sm" variant="destructive" onClick={() => act("forget", { relay: relay.id }, `Removed ${relay.name}`, refresh)}>
				Forget
			</Button>
			<Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
				Cancel
			</Button>
		</span>
	);
}
