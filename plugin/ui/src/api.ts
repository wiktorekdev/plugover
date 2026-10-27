export type Kind = "wheel" | "pedals" | "joystick" | "gamepad" | "keyboard" | "mouse" | "other";

export interface Device {
	busid: string;
	/** vendor:product */
	id: string;
	title: string;
	name: string;
	maker: string;
	kind: Kind;
	source: "catalog" | "usbids" | "device" | "unknown";
	blocked: string | null;
	lent_to: string | null;
	state: "here" | "free" | "elsewhere";
	auto: boolean;
}

export interface Relay {
	id: string;
	name: string;
	address: string;
	online: boolean;
	devices: Device[];
}

export interface Status {
	host: string;
	session: boolean;
	relays: Relay[];
}

export interface Found {
	id: string;
	name: string;
	paired: boolean;
	addresses: string[];
}

export class ApiError extends Error {
	constructor(
		message: string,
		/** The daemon on this computer is not running. */
		readonly offline: boolean,
	) {
		super(message);
	}
}

/** Relative path: the console serves this page under /plugin-ui/plugover/. */
export async function call<T>(op: string, body?: object): Promise<T> {
	const res = await fetch(`api/${op}`, {
		method: body ? "POST" : "GET",
		headers: body ? { "content-type": "application/json" } : undefined,
		body: body ? JSON.stringify(body) : undefined,
	});
	const data = await res.json().catch(() => ({ ok: false, error: `The page could not reach the plugin (HTTP ${res.status}).` }));
	if (!data.ok) throw new ApiError(data.error ?? "Something went wrong.", Boolean(data.offline));
	return data as T;
}

export const KIND_LABEL: Record<Kind, string> = {
	wheel: "Steering wheel",
	pedals: "Pedals",
	joystick: "Flight stick",
	gamepad: "Controller",
	keyboard: "Keyboard",
	mouse: "Mouse",
	other: "USB device",
};

export const SOURCE_LABEL: Record<Device["source"], string> = {
	catalog: "Plugover catalog",
	usbids: "usb.ids",
	device: "Device descriptor",
	unknown: "Unknown",
};
