import { describe, expect, it } from "vitest";
import {
	areaContextAction,
	buildElementCandidates,
	cycleElementLevel,
	elementCandidateSignature,
	hasAreaSelection,
} from "./element-selection";

const bounds = (x: number, y: number, width: number, height: number) => ({
	position: { x, y },
	size: { width, height },
});

describe("buildElementCandidates", () => {
	it("uses the nearest valid parent for recording", () => {
		const result = buildElementCandidates(
			[
				bounds(20, 20, 40, 20),
				bounds(10, 10, 149.9, 180),
				bounds(5, 5, 180, 180),
				bounds(0, 0, 400, 300),
			],
			bounds(0, 0, 400, 300),
			{ width: 150, height: 150 },
		);

		expect(result).toEqual([
			{ x: 5, y: 5, width: 180, height: 180 },
			{ x: 0, y: 0, width: 400, height: 300 },
		]);
	});

	it("keeps small controls for screenshots", () => {
		const result = buildElementCandidates([bounds(20, 20, 40, 20)], undefined, {
			width: 1,
			height: 1,
		});

		expect(result).toEqual([{ x: 20, y: 20, width: 40, height: 20 }]);
	});

	it("deduplicates matching element and window bounds", () => {
		const result = buildElementCandidates(
			[bounds(0, 0, 400, 300)],
			bounds(0, 0, 400, 300),
			{ width: 1, height: 1 },
		);

		expect(result).toHaveLength(1);
	});
});

describe("element level selection", () => {
	it("cycles toward parents and back toward children", () => {
		expect(cycleElementLevel(0, -1, 3)).toBe(1);
		expect(cycleElementLevel(1, 1, 3)).toBe(0);
	});

	it("keeps the first and last levels within bounds", () => {
		expect(cycleElementLevel(0, 1, 3)).toBe(0);
		expect(cycleElementLevel(2, -1, 3)).toBe(2);
		expect(cycleElementLevel(1, 0, 3)).toBe(1);
		expect(cycleElementLevel(0, -1, 0)).toBe(0);
	});

	it("changes its signature when the hierarchy changes", () => {
		const first = [{ x: 0, y: 0, width: 100, height: 100 }];
		const second = [{ x: 1, y: 0, width: 100, height: 100 }];
		expect(elementCandidateSignature(first)).not.toBe(
			elementCandidateSignature(second),
		);
	});
});

describe("area selection state", () => {
	it("distinguishes an existing selection from the empty crop", () => {
		expect(hasAreaSelection({ x: 0, y: 0, width: 200, height: 100 })).toBe(
			true,
		);
		expect(hasAreaSelection({ x: 0, y: 0, width: 1, height: 1 })).toBe(false);
	});

	it("maps right click to exit before selection and clear after selection", () => {
		expect(areaContextAction({ x: 0, y: 0, width: 1, height: 1 })).toBe("exit");
		expect(areaContextAction({ x: 10, y: 10, width: 200, height: 100 })).toBe(
			"clear",
		);
	});
});
