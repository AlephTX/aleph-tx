import { createContext, useContext, useEffect, useState } from 'react'

/** Shared wall clock, ticking once a second, so render stays pure. */
export const NowContext = createContext(0)

export function useTicker(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs)
    return () => clearInterval(id)
  }, [intervalMs])
  return now
}

export const useNow = () => useContext(NowContext)
