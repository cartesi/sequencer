import { createBrowserRouter } from "react-router-dom";

import { Shell } from "./Shell";
import { MonitorPage } from "../pages/MonitorPage";
import { TxSenderPage } from "../pages/TxSenderPage";
import { WsFeedPage } from "../pages/WsFeedPage";

export const router = createBrowserRouter([
  {
    path: "/",
    element: <Shell />,
    children: [
      {
        index: true,
        element: <MonitorPage />,
      },
      {
        path: "/monitor",
        element: <MonitorPage />,
      },
      {
        path: "/tx-sender",
        element: <TxSenderPage />,
      },
      {
        path: "/ws-feed",
        element: <WsFeedPage />,
      },
    ],
  },
]);
