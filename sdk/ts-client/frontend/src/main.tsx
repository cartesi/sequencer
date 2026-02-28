import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createConfig, http, WagmiProvider, type Chain } from "wagmi";
import { injected } from "wagmi/connectors";
import { mainnet, sepolia } from "wagmi/chains";

import { App } from "./app/App";
import { SequencerSettingsProvider } from "./state/sequencerUrl";
import { TxSenderStateProvider } from "./state/txSender";
import "./styles.css";

const local31337: Chain = {
  id: 31337,
  name: "Local 31337",
  nativeCurrency: {
    name: "Ether",
    symbol: "ETH",
    decimals: 18,
  },
  rpcUrls: {
    default: { http: ["http://127.0.0.1:8545"] },
    public: { http: ["http://127.0.0.1:8545"] },
  },
};

const chains = [local31337, mainnet, sepolia] as const;

const wagmiConfig = createConfig({
  chains,
  connectors: [injected()],
  transports: {
    [local31337.id]: http(),
    [mainnet.id]: http(),
    [sepolia.id]: http(),
  },
});

const queryClient = new QueryClient();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <WagmiProvider config={wagmiConfig}>
      <QueryClientProvider client={queryClient}>
        <SequencerSettingsProvider>
          <TxSenderStateProvider>
            <App />
          </TxSenderStateProvider>
        </SequencerSettingsProvider>
      </QueryClientProvider>
    </WagmiProvider>
  </React.StrictMode>,
);
