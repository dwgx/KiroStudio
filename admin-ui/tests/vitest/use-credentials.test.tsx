/**
 * `useCredentials` / `useSetDisabled` — React Query hooks over the admin axios client.
 *
 * settings toForm/diff 已抽到 `@/lib/settings-form`（vitest/settings-form.test.ts）。
 * 本文件测 hooks + mocked axios。shouldClearAdminSession 仍走 node:test。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && pnpm test
 * ```
 */
import { beforeEach, describe, expect, test, vi } from 'vitest'
import { renderHook, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'

const { get, post } = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
}))

vi.mock('axios', () => {
  const instance = {
    get,
    post,
    put: vi.fn(),
    delete: vi.fn(),
    interceptors: {
      request: { use: vi.fn() },
      response: { use: vi.fn() },
    },
  }
  const create = vi.fn(() => instance)
  return { default: { create }, create }
})

import { useCredentials, useSetDisabled } from '@/hooks/use-credentials'

const CREDENTIALS_STATUS = {
  total: 1,
  available: 1,
  currentId: 7,
  credentials: [],
}

function makeWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: Infinity },
      mutations: { retry: false },
    },
  })
  return function Wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  }
}

describe('use-credentials', () => {
  beforeEach(() => {
    get.mockReset()
    post.mockReset()
  })

  test('useCredentials GETs /credentials and surfaces the payload', async () => {
    get.mockResolvedValue({ data: CREDENTIALS_STATUS })

    const { result } = renderHook(() => useCredentials(), { wrapper: makeWrapper() })

    await waitFor(() => expect(result.current.isSuccess).toBe(true))
    expect(get).toHaveBeenCalledWith('/credentials')
    expect(result.current.data).toEqual(CREDENTIALS_STATUS)
  })

  test('useSetDisabled POSTs then invalidates the credentials query', async () => {
    get.mockResolvedValue({ data: CREDENTIALS_STATUS })
    post.mockResolvedValue({ data: { success: true, message: 'ok' } })

    const { result } = renderHook(
      () => ({ list: useCredentials(), setDisabled: useSetDisabled() }),
      { wrapper: makeWrapper() },
    )

    await waitFor(() => expect(result.current.list.isSuccess).toBe(true))
    expect(get).toHaveBeenCalledWith('/credentials')
    get.mockClear()
    get.mockResolvedValue({ data: CREDENTIALS_STATUS })

    result.current.setDisabled.mutate({ id: 7, disabled: true })

    await waitFor(() => expect(result.current.setDisabled.isSuccess).toBe(true))
    expect(post).toHaveBeenCalledWith('/credentials/7/disabled', { disabled: true })
    await waitFor(() => expect(get).toHaveBeenCalledWith('/credentials'))
  })
})
