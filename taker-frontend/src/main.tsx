import { ChakraProvider } from "@chakra-ui/react";
import React from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { App } from "./App";
import "./index.css";
import theme from "./theme";

const container = document.getElementById("root");
const root = createRoot(container!);
root.render(
    <React.StrictMode>
        <ChakraProvider theme={theme}>
            <BrowserRouter>
                <App />
            </BrowserRouter>
        </ChakraProvider>
    </React.StrictMode>,
);
