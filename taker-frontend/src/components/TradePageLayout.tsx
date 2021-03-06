import {
    Accordion,
    AccordionButton,
    AccordionIcon,
    AccordionItem,
    AccordionPanel,
    Box,
    Button,
    ButtonGroup,
    Center,
    HStack,
    Text,
    useColorModeValue,
    VStack,
} from "@chakra-ui/react";
import * as React from "react";
import { Link as ReachLink, Outlet, useLocation } from "react-router-dom";
import { VIEWPORT_WIDTH_PX } from "../App";
import { Cfd, ConnectionStatus, isClosed } from "../types";
import History from "./History";
import PromoBanner from "./PromoBanner";

interface TradePageLayoutProps {
    cfds: Cfd[];
    connectedToMaker: ConnectionStatus;
    showPromoBanner: boolean;
    showExtraInfo: boolean;
}

export function TradePageLayout({ cfds, connectedToMaker, showPromoBanner, showExtraInfo }: TradePageLayoutProps) {
    return (
        <VStack w={"100%"}>
            {showPromoBanner && <PromoBanner />}
            <NavigationButtons />
            <Outlet />
            <HistoryLayout cfds={cfds} connectedToMaker={connectedToMaker} showExtraInfo={showExtraInfo} />
        </VStack>
    );
}

interface HistoryLayoutProps {
    cfds: Cfd[];
    connectedToMaker: ConnectionStatus;
    showExtraInfo: boolean;
}

function HistoryLayout({ cfds, connectedToMaker, showExtraInfo }: HistoryLayoutProps) {
    const closedPositions = cfds.filter((cfd) => isClosed(cfd));

    return (
        <VStack padding={3} w={"100%"}>
            <History
                connectedToMaker={connectedToMaker}
                cfds={cfds.filter((cfd) => !isClosed(cfd))}
                showExtraInfo={showExtraInfo}
            />

            {closedPositions.length > 0
                && (
                    <Accordion allowToggle maxWidth={VIEWPORT_WIDTH_PX} width={"100%"}>
                        <AccordionItem>
                            <h2>
                                <AccordionButton>
                                    <AccordionIcon />
                                    <Box w={"100%"} textAlign="center">
                                        Show Closed Positions
                                    </Box>
                                    <AccordionIcon />
                                </AccordionButton>
                            </h2>
                            <AccordionPanel pb={4}>
                                <History
                                    cfds={closedPositions}
                                    connectedToMaker={connectedToMaker}
                                    showExtraInfo={showExtraInfo}
                                />
                            </AccordionPanel>
                        </AccordionItem>
                    </Accordion>
                )}
        </VStack>
    );
}

function NavigationButtons() {
    const location = useLocation();
    const isLongSelected = location.pathname.includes("long");
    const isShortSelected = !isLongSelected;

    const unSelectedButton = "transparent";
    const selectedButton = useColorModeValue("grey.400", "black.400");
    const buttonBorder = useColorModeValue("grey.400", "black.400");
    const buttonText = useColorModeValue("black", "white");

    return (
        <HStack>
            <Center>
                <ButtonGroup
                    padding="3"
                    spacing="6"
                    id={"longShortButtonSwitch"}
                >
                    <Button
                        as={ReachLink}
                        to="/long"
                        color={isLongSelected ? selectedButton : unSelectedButton}
                        bg={isLongSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        isActive={isLongSelected}
                        size="lg"
                        h={10}
                        w={"40"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Long BTC</Text>
                    </Button>
                    <Button
                        as={ReachLink}
                        to="/short"
                        color={isShortSelected ? selectedButton : unSelectedButton}
                        bg={isShortSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        isActive={isShortSelected}
                        size="lg"
                        h={10}
                        w={"40"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Short BTC</Text>
                    </Button>
                </ButtonGroup>
            </Center>
        </HStack>
    );
}
