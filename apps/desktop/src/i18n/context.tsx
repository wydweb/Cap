import {
	createContext,
	createEffect,
	createMemo,
	createSignal,
	onCleanup,
	onMount,
	type ParentProps,
	useContext,
} from "solid-js";
import {
	isLocalePreference,
	type LocalePreference,
	type MessageKey,
	type MessageParams,
	resolveLocale,
	type SupportedLocale,
	translate,
} from "./core";

const LOCALE_STORAGE_KEY = "cap-interface-locale";

type I18nContextValue = {
	locale: () => SupportedLocale;
	preference: () => LocalePreference;
	setPreference: (preference: LocalePreference) => void;
	t: (key: MessageKey, params?: MessageParams) => string;
	formatDate: (
		value: Date | number,
		options?: Intl.DateTimeFormatOptions,
	) => string;
	formatNumber: (value: number, options?: Intl.NumberFormatOptions) => string;
};

const I18nContext = createContext<I18nContextValue>();

function browserLanguages() {
	return typeof navigator === "undefined" ? ["en"] : navigator.languages;
}

function storedPreference(): LocalePreference {
	if (typeof localStorage === "undefined") return "system";
	try {
		const stored = localStorage.getItem(LOCALE_STORAGE_KEY);
		return isLocalePreference(stored) ? stored : "system";
	} catch {
		return "system";
	}
}

export function I18nProvider(props: ParentProps) {
	const [preference, setPreferenceSignal] = createSignal<LocalePreference>(
		storedPreference(),
	);
	const [languages, setLanguages] = createSignal(browserLanguages());
	const locale = createMemo(() => resolveLocale(preference(), languages()));

	function setPreference(next: LocalePreference) {
		setPreferenceSignal(next);
		try {
			localStorage.setItem(LOCALE_STORAGE_KEY, next);
		} catch {}
	}

	createEffect(() => {
		if (typeof document === "undefined") return;
		document.documentElement.lang = locale();
		document.documentElement.dir = "ltr";
	});

	onMount(() => {
		const handleLanguageChange = () => setLanguages(browserLanguages());
		window.addEventListener("languagechange", handleLanguageChange);
		onCleanup(() =>
			window.removeEventListener("languagechange", handleLanguageChange),
		);
	});

	const value: I18nContextValue = {
		locale,
		preference,
		setPreference,
		t: (key, params) => translate(locale(), key, params),
		formatDate: (value, options) =>
			new Intl.DateTimeFormat(locale(), options).format(value),
		formatNumber: (value, options) =>
			new Intl.NumberFormat(locale(), options).format(value),
	};

	return (
		<I18nContext.Provider value={value}>{props.children}</I18nContext.Provider>
	);
}

export function useI18n() {
	const context = useContext(I18nContext);
	if (!context) throw new Error("useI18n must be used within I18nProvider");
	return context;
}
