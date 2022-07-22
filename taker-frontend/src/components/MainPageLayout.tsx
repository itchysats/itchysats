import { Box, Center, useColorModeValue } from "@chakra-ui/react";
import * as React from "react";
import { Outlet } from "react-router-dom";
import { SemVer } from "semver";
import { BG_DARK, BG_LIGHT, FOOTER_HEIGHT, HEADER_HEIGHT, VIEWPORT_WIDTH, VIEWPORT_WIDTH_PX } from "../App";
import { ConnectionStatus, IdentityInfo, WalletInfo } from "../types";
import Footer from "./Footer";
import IncompatibleWarning from "./IncompatibleWarning";
import Nav from "./NavBar";
import OutdatedWarning from "./OutdatedWarning";

type MainPageProps = {
    outdatedWarningIsVisible: boolean;
    incompatibleWarningIsVisible: boolean;
    githubVersion: SemVer | null | undefined;
    daemonVersion: SemVer | null | undefined;
    onCloseOutdatedWarning: () => void;
    onCloseIncompatibleWarning: () => void;
    walletInfo: WalletInfo | null;
    connectedToMaker: ConnectionStatus;
    nextFundingEvent: string | null;
    referencePrice: number | undefined;
    identityOrUndefined: IdentityInfo | null;
    setExtraInfo: (val: boolean) => void;
    showExtraInfo: boolean;
};

export function MainPageLayout(
    {
        outdatedWarningIsVisible,
        incompatibleWarningIsVisible,
        githubVersion,
        daemonVersion,
        onCloseOutdatedWarning,
        onCloseIncompatibleWarning,
        walletInfo,
        connectedToMaker,
        nextFundingEvent,
        referencePrice,
        identityOrUndefined,
        setExtraInfo,
        showExtraInfo,
    }: MainPageProps,
) {
    return (
        <>
            {outdatedWarningIsVisible
                && (
                    <OutdatedWarning
                        githubVersion={githubVersion}
                        daemonVersion={daemonVersion}
                        onClose={onCloseOutdatedWarning}
                    />
                )}

            {incompatibleWarningIsVisible
                && <IncompatibleWarning onClose={onCloseIncompatibleWarning} />}

            <Nav
                walletInfo={walletInfo}
                connectedToMaker={connectedToMaker}
                nextFundingEvent={nextFundingEvent}
                referencePrice={referencePrice}
            />
            <Center>
                <Box
                    maxWidth={(VIEWPORT_WIDTH + 200) + "px"}
                    width={"100%"}
                    bgGradient={useColorModeValue(
                        "linear(to-r, white 5%, gray.800, white 95%)",
                        "linear(to-r, gray.800 5%, white, gray.800 95%)",
                    )}
                >
                    <Center>
                        <Box
                            textAlign="center"
                            padding={3}
                            bg={useColorModeValue(BG_LIGHT, BG_DARK)}
                            maxWidth={VIEWPORT_WIDTH_PX}
                            marginTop={`${HEADER_HEIGHT}px`}
                            minHeight={`calc(100vh - ${FOOTER_HEIGHT}px - ${HEADER_HEIGHT}px)`}
                            width={"100%"}
                        >
                            <Outlet />
                        </Box>
                    </Center>
                </Box>
            </Center>

            <Footer
                identityInfo={identityOrUndefined}
                daemonVersion={daemonVersion?.version}
                githubVersion={githubVersion?.version}
                onExtraInfoToggle={setExtraInfo}
                showExtraInfo={showExtraInfo}
            />
        </>
    );
}
