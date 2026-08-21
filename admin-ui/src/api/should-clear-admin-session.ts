/**
 * Whether an admin API error should drop the stored adminApiKey and reload.
 *
 * 401 = key invalid (auth middleware). 403 is also used for business rejection
 * (`import_keys` off, IP allow/block), so only treat 403 as session death when
 * the body is `authentication_error`.
 */
export function shouldClearAdminSession(
  status: number | undefined,
  errorType: string | undefined,
): boolean {
  if (status === 401) return true
  return status === 403 && errorType === 'authentication_error'
}
