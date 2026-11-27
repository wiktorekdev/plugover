/** Only the most recently started refresh can apply a success or failure. */
export function latestRefresh<T>(load: () => Promise<T>, apply: (result: T) => void) {
	let latest = 0;

	return {
		refresh: async () => {
			const request = ++latest;
			const result = await load();
			if (request === latest) apply(result);
		},
		invalidate: () => {
			latest++;
		},
	};
}
