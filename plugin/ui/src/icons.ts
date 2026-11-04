import { Bluetooth, Gamepad2, Headphones, Joystick, Keyboard, type LucideIcon, Mouse, Usb, Webcam, createLucideIcon } from "lucide-react";
import type { Kind } from "./api";

// Lucide has no steering wheel, shifter or pedals. The wheel and the shifter are steeringWheel
// and gearbox from @lucide/lab (ISC, by the Lucide authors), copied rather than installed for two
// icons. The pedals are ours on the same grid (24x24, 2px round strokes, 2px padding). All three
// are built with Lucide's own factory, so they size and render like the rest of the set.
const SteeringWheel = createLucideIcon("steering-wheel", [
	["circle", { cx: "12", cy: "12", r: "10", key: "rim" }],
	["path", { d: "m3.3 7 7 4", key: "left" }],
	["path", { d: "m13.7 11 7-4", key: "right" }],
	["path", { d: "M12 14v8", key: "down" }],
	["circle", { cx: "12", cy: "12", r: "2", key: "hub" }],
]);

const Shifter = createLucideIcon("gearbox", [
	["path", { d: "M5 4v16", key: "left" }],
	["path", { d: "M12 4v16", key: "middle" }],
	["path", { d: "M19 4v8H5", key: "right" }],
]);

const Pedals = createLucideIcon("pedals", [
	["rect", { x: "3", y: "9", width: "5", height: "9", rx: "1.5", key: "clutch" }],
	["rect", { x: "10", y: "9", width: "5", height: "9", rx: "1.5", key: "brake" }],
	["rect", { x: "17", y: "5", width: "4", height: "13", rx: "1.5", key: "throttle" }],
	["path", { d: "M2 21h20", key: "floor" }],
]);

export const KIND_ICON: Record<Kind, LucideIcon> = {
	wheel: SteeringWheel,
	pedals: Pedals,
	shifter: Shifter,
	joystick: Joystick,
	gamepad: Gamepad2,
	keyboard: Keyboard,
	mouse: Mouse,
	audio: Headphones,
	camera: Webcam,
	bluetooth: Bluetooth,
	other: Usb,
};
