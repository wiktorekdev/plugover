import { Accordion, AccordionContent, AccordionItem, AccordionTrigger } from "@unom/ui/accordion";
import { Badge } from "@unom/ui/badge";
import { Button } from "@unom/ui/button";
import Card from "@unom/ui/card";
import { EmptyState } from "@unom/ui/empty-state";
import { InputText } from "@unom/ui/form/input-text";
import { Switch } from "@unom/ui/form/switch";
import { cn } from "@unom/ui/lib/utils";
import { Spinner } from "@unom/ui/spinner";
import { Toaster, toast } from "@unom/ui/toast";
import { ChevronDown, Monitor, Unplug } from "lucide-react";
import { type FormEvent, useCallback, useEffect, useState } from "react";
import { ApiError, type Device, type Found, KIND_LABEL, type Relay, SOURCE_LABEL, type Status, call } from "./api";
import { KIND_ICON } from "./icons";

const POLL_MS = 2000;
const EMBEDDED = window.self !== window.top;

export function App() {
	const [status, setStatus] = useState<Status>();
	const [error, setError] = useState<ApiError>();

	const refresh = useCallback(async () => {
		try {
			setStatus(await call<Status>("status"));
			setError(undefined);
		} catch (e) {
			setError(e instanceof ApiError ? e : new ApiError(String(e), false));
		}
	}, []);

	useEffect(() => {
		void refresh();
		const timer = setInterval(refresh, POLL_MS);
		return () => clearInterval(timer);
	}, [refresh]);

	let content: React.ReactNode;
	if (error?.offline) {
		content = (
			<EmptyState
				icon={Unplug}
				variant="error"
				title="Plugover host daemon is not running"
				description="Install it on this computer with install.sh host (Linux) or install.ps1 (Windows)."
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
	} else if (status.relays.length === 0) {
		content = <Setup onPaired={refresh} />;
	} else {
		content = status.relays.map((r) => <RelayView key={r.id} relay={r} streaming={status.session} refresh={refresh} />);
	}

	return (
		<main className="mx-auto flex max-w-3xl flex-col gap-10 px-4 py-8 sm:px-6">
			{content}
			<Toaster />
		</main>
	);
}

function Setup({ onPaired }: { onPaired: () => void }) {
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
						{found.map((f) => (
							<li key={f.id} className="flex items-center gap-4 border-b border-border p-4 last:border-0">
								<Monitor className="size-5 text-primary" />
								<div className="min-w-0 flex-1">
									<p className="font-medium">{f.name}</p>
									<p className="text-sm text-muted-foreground">{f.addresses[0]}</p>
								</div>
								<Button disabled={!f.addresses[0]} onClick={() => pair(f.name, f.addresses[0])}>
									Pair
								</Button>
							</li>
						))}
					</ul>
				)}
			</Card>

			<div className="flex flex-wrap items-end justify-between gap-4">
				<form onSubmit={submit} className="flex flex-wrap items-end gap-2">
					<label className="flex flex-col gap-1 text-sm">
						<span className="text-muted-foreground">Relay address</span>
						<InputText value={address} onChange={(e) => setAddress(e.target.value)} placeholder="192.168.1.50" />
					</label>
					<Button type="submit" variant="secondary">
						Pair
					</Button>
				</form>
				<Button variant="ghost" onClick={discover}>
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
		<Card className="flex flex-col items-center gap-5 py-12 text-center">
			<h1 className="max-w-md text-2xl font-semibold tracking-tight">Pairing code for {name}</h1>
			<p className="max-w-md text-muted-foreground">
				Shown at the end of the relay install. For a new one, run <code className="whitespace-nowrap rounded bg-muted px-1.5 py-0.5">plugover code</code> on{" "}
				{name} as root or administrator.
			</p>
			<form onSubmit={submit} className="flex flex-wrap items-end justify-center gap-2">
				<InputText
					value={code}
					onChange={(e) => setCode(e.target.value)}
					placeholder="482 913"
					inputMode="numeric"
					autoComplete="one-time-code"
					aria-label="Pairing code"
					autoFocus
					className="w-40 text-center font-mono text-lg tracking-widest"
				/>
				<Button type="submit" disabled={busy || digits.length !== 6}>
					{busy ? <Spinner /> : "Pair"}
				</Button>
				<Button type="button" variant="ghost" onClick={onCancel}>
					Back
				</Button>
			</form>
			{failure && <p className="max-w-md text-destructive">Pairing failed: {failure}</p>}
		</Card>
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

function RelayView({ relay, streaming, refresh }: { relay: Relay; streaming: boolean; refresh: () => Promise<void> }) {
	const shareable = relay.devices.filter((d) => !d.blocked);
	const hidden = relay.devices.length - shareable.length;
	const chosen = shareable.filter((d) => d.auto);
	const others = shareable.filter((d) => !d.auto);

	return (
		<section className="flex flex-col gap-6">
			<header className="flex flex-wrap items-end justify-between gap-3">
				<div>
					{/* Inside the console its page header already names the plugin. */}
					{!EMBEDDED && <h1 className="mb-1 text-2xl font-semibold tracking-tight">USB devices</h1>}
					<p className="text-lg font-medium">{relay.name}</p>
				</div>
				<div className="flex gap-2">
					<Badge dot variant={relay.online ? "success" : "warn"}>
						{relay.online ? "Online" : "Offline"}
					</Badge>
					<Badge dot pulse={streaming} variant={streaming ? "default" : "outline"}>
						{streaming ? "Stream running" : "No stream"}
					</Badge>
				</div>
			</header>

			{!relay.online && (
				<Card muted>
					<p className="text-muted-foreground">
						{relay.name} is not responding at {relay.address}.
					</p>
				</Card>
			)}

			<div className="flex flex-col gap-2">
				<h2 className="text-sm font-medium text-muted-foreground">Auto-connect</h2>
				<Card padding={false}>
					{chosen.length > 0 ? (
						chosen.map((d) => <DeviceRow key={d.busid} relay={relay} device={d} refresh={refresh} />)
					) : (
						<p className="p-5 text-muted-foreground">
							No devices yet. Turn on auto-connect for a device below.
						</p>
					)}
				</Card>
			</div>

			{others.length > 0 && (
				<Accordion type="single" collapsible defaultValue={chosen.length > 0 ? undefined : "others"}>
					<AccordionItem value="others">
						<AccordionTrigger>
							Other devices ({others.length})
						</AccordionTrigger>
						<AccordionContent>
							<Card padding={false}>
								{others.map((d) => (
									<DeviceRow key={d.busid} relay={relay} device={d} refresh={refresh} />
								))}
							</Card>
						</AccordionContent>
					</AccordionItem>
				</Accordion>
			)}

			<footer className="flex flex-wrap items-center justify-between gap-3 text-sm text-muted-foreground">
				<span>{hidden > 0 && `${hidden} hidden (hubs and storage are never shared)`}</span>
				<ForgetButton relay={relay} refresh={refresh} />
			</footer>
		</section>
	);
}

function KindTile({ device }: { device: Device }) {
	const Icon = KIND_ICON[device.kind] ?? KIND_ICON.other;
	const here = device.state === "here";
	return (
		<div
			className={cn(
				"grid size-11 shrink-0 place-items-center rounded-xl transition-colors",
				here ? "bg-primary text-primary-foreground" : "bg-neutral-accent text-primary",
			)}
		>
			<Icon className="size-6" />
		</div>
	);
}

function DeviceRow({ relay, device: d, refresh }: { relay: Relay; device: Device; refresh: () => Promise<void> }) {
	const [open, setOpen] = useState(false);
	const [busy, setBusy] = useState(false);
	const here = d.state === "here";
	const elsewhere = d.state === "elsewhere";
	const run = async (op: string, body: object, done: string) => {
		setBusy(true);
		await act(op, { relay: relay.id, ...body }, done, refresh);
		setBusy(false);
	};

	return (
		<div className="border-b border-border last:border-0">
			<div className="flex flex-wrap items-center gap-x-4 gap-y-3 p-4">
				<KindTile device={d} />
				<div className="min-w-40 flex-1">
					<p className="truncate font-medium">{d.title}</p>
					<div className="mt-0.5 flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
						{KIND_LABEL[d.kind]}
						{here && <Badge size="sm">Connected</Badge>}
						{elsewhere && (
							<Badge size="sm" variant="warn">
								In use by {d.lent_to}
							</Badge>
						)}
					</div>
				</div>
				<label className="flex items-center gap-2 text-sm text-muted-foreground">
					Auto-connect
					<Switch
						checked={d.auto}
						disabled={busy}
						onCheckedChange={(on) =>
							run("auto", { device: d.id, on }, on ? `${d.title} connects when a stream starts` : `Auto-connect off for ${d.title}`)
						}
					/>
				</label>
				<Button
					size="sm"
					variant={here ? "outline" : "secondary"}
					disabled={busy || elsewhere || !relay.online}
					onClick={() =>
						here
							? run("detach", { busid: d.busid }, `${d.title} disconnected`)
							: run("attach", { busid: d.busid }, `${d.title} connected`)
					}
				>
					{here ? "Disconnect" : "Connect"}
				</Button>
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
				</dl>
			)}
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
		<span className="flex items-center gap-2">
			Forget {relay.name}? You will need to pair again.
			<Button size="sm" variant="destructive" onClick={() => act("forget", { relay: relay.id }, `Removed ${relay.name}`, refresh)}>
				Forget
			</Button>
			<Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
				Cancel
			</Button>
		</span>
	);
}
