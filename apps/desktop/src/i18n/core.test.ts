import { describe, expect, it } from "vitest";
import {
	isLocalePreference,
	resolveLocale,
	resolveSystemLocale,
	translate,
} from "./core";

describe("desktop i18n", () => {
	it("resolves Chinese system locales", () => {
		expect(resolveSystemLocale(["zh-Hans-CN", "en-US"])).toBe("zh-CN");
		expect(resolveSystemLocale(["en-US"])).toBe("en");
	});

	it("honors an explicit locale preference", () => {
		expect(resolveLocale("en", ["zh-CN"])).toBe("en");
		expect(resolveLocale("system", ["zh-CN"])).toBe("zh-CN");
	});

	it("validates stored locale preferences", () => {
		expect(isLocalePreference("system")).toBe(true);
		expect(isLocalePreference("zh-CN")).toBe(true);
		expect(isLocalePreference("fr")).toBe(false);
	});

	it("translates messages and interpolates parameters", () => {
		expect(translate("zh-CN", "common.cancel")).toBe("取消");
		expect(
			translate("zh-CN", "target.minimumSize", {
				width: 150,
				height: 150,
			}),
		).toBe("最小尺寸为 150 × 150");
		expect(translate("en", "common.error", { unused: "ignored" })).toBe(
			"Error",
		);
	});
});
