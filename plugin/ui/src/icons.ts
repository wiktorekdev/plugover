import { Gamepad2, Joystick, Keyboard, type LucideIcon, Mouse, Usb, createLucideIcon } from "lucide-react";
import type { Kind } from "./api";

// Lucide has no steering wheel or pedals. These two follow its grid (24x24, 2px round
// strokes, 2px padding) and are built with its own factory, so they size and render like the
// rest of the set.
const SteeringWheel = createLucideIcon("steering-wheel", [
	["circle", { cx: "12", cy: "12", r: "10", key: "rim" }],
	["circle", { cx: "12", cy: "12", r: "2", key: "hub" }],
	["path", { d: "M2 12h8", key: "left" }],
	["path", { d: "M14 12h8", key: "right" }],
	["path", { d: "M12 14v8", key: "down" }],
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
	joystick: Joystick,
	gamepad: Gamepad2,
	keyboard: Keyboard,
	mouse: Mouse,
	other: Usb,
};
