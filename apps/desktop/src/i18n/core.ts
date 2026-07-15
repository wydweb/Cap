import { enMessages, type MessageKey } from "./messages/en";
import { zhCNMessages } from "./messages/zh-CN";

export type { MessageKey } from "./messages/en";

export const SUPPORTED_LOCALES = ["en", "zh-CN"] as const;
export type SupportedLocale = (typeof SUPPORTED_LOCALES)[number];
export type LocalePreference = SupportedLocale | "system";
export type MessageParams = Record<string, string | number>;

const dictionaries: Record<
	SupportedLocale,
	Partial<Record<MessageKey, string>>
> = {
	en: enMessages,
	"zh-CN": zhCNMessages,
};

export function isLocalePreference(value: unknown): value is LocalePreference {
	return (
		value === "system" || SUPPORTED_LOCALES.includes(value as SupportedLocale)
	);
}

export function resolveSystemLocale(
	languages: readonly string[],
): SupportedLocale {
	return languages.some((language) => language.toLowerCase().startsWith("zh"))
		? "zh-CN"
		: "en";
}

export function resolveLocale(
	preference: LocalePreference,
	languages: readonly string[],
): SupportedLocale {
	return preference === "system" ? resolveSystemLocale(languages) : preference;
}

export function translate(
	locale: SupportedLocale,
	key: MessageKey,
	params?: MessageParams,
) {
	const template = dictionaries[locale][key] ?? enMessages[key];
	if (!params) return template;
	return template.replace(/\{(\w+)\}/g, (match, name: string) =>
		Object.hasOwn(params, name) ? String(params[name]) : match,
	);
}
