import { describe, expect, it } from "vitest";
import {
	calculateBoundsLabelPosition,
	fitRatioToLongEdge,
	fitSizeWithinAvailableBounds,
	resolveVisualBounds,
} from "./cropper-visuals";

const label = { width: 80, height: 24 };
const viewport = { width: 1000, height: 800 };

describe("fitRatioToLongEdge", () => {
	it("uses width when it is the longer dimension", () => {
		expect(fitRatioToLongEdge(320, 180, 1)).toEqual({
			width: 320,
			height: 320,
		});
	});

	it("uses height when it is the longer dimension", () => {
		expect(fitRatioToLongEdge(180, 320, 1)).toEqual({
			width: 320,
			height: 320,
		});
	});
});

describe("fitSizeWithinAvailableBounds", () => {
	it("uses the remaining vertical space when height reaches the edge first", () => {
		expect(
			fitSizeWithinAvailableBounds(
				{ width: 320, height: 320 },
				{ width: 500, height: 200 },
			),
		).toEqual({ width: 200, height: 200 });
	});

	it("uses the remaining horizontal space when width reaches the edge first", () => {
		expect(
			fitSizeWithinAvailableBounds(
				{ width: 320, height: 320 },
				{ width: 180, height: 400 },
			),
		).toEqual({ width: 180, height: 180 });
	});

	it("keeps the requested size when both directions have enough space", () => {
		expect(
			fitSizeWithinAvailableBounds(
				{ width: 320, height: 180 },
				{ width: 500, height: 400 },
			),
		).toEqual({ width: 320, height: 180 });
	});
});

describe("resolveVisualBounds", () => {
	const selection = { x: 10, y: 20, width: 300, height: 200 };
	const preview = { x: 400, y: 100, width: 200, height: 150 };

	it("uses the active selection while interacting", () => {
		expect(resolveVisualBounds(true, preview, selection)).toBe(selection);
	});

	it("uses the hovered element while idle", () => {
		expect(resolveVisualBounds(false, preview, selection)).toBe(preview);
	});

	it("returns to the selection when the preview disappears", () => {
		expect(resolveVisualBounds(false, undefined, selection)).toBe(selection);
	});
});

describe("calculateBoundsLabelPosition", () => {
	it("places the label above the selection by default", () => {
		expect(
			calculateBoundsLabelPosition(
				{ x: 100, y: 100, width: 300, height: 200 },
				label,
				viewport,
			),
		).toEqual({ x: 100, y: 68, placement: "top-left-outside" });
	});

	it("uses the right edge when there is no room above", () => {
		expect(
			calculateBoundsLabelPosition(
				{ x: 100, y: 4, width: 300, height: 200 },
				label,
				viewport,
			),
		).toEqual({ x: 408, y: 8, placement: "top-right-outside" });
	});

	it("uses the bottom edge when the top and right do not fit", () => {
		expect(
			calculateBoundsLabelPosition(
				{ x: 700, y: 4, width: 292, height: 200 },
				label,
				viewport,
			),
		).toEqual({ x: 700, y: 212, placement: "bottom-left-outside" });
	});

	it("uses the inside corner for a full-screen selection", () => {
		expect(
			calculateBoundsLabelPosition(
				{ x: 0, y: 0, width: 1000, height: 800 },
				label,
				viewport,
			),
		).toEqual({ x: 8, y: 8, placement: "top-left-inside" });
	});

	it("keeps the label within the viewport", () => {
		const position = calculateBoundsLabelPosition(
			{ x: 990, y: 100, width: 10, height: 200 },
			label,
			viewport,
		);

		expect(position.x).toBeLessThanOrEqual(912);
		expect(position.y).toBeGreaterThanOrEqual(8);
	});
});
