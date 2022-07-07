import { Icon, MoonIcon, SunIcon, WarningIcon } from "@chakra-ui/icons";
import {
    Box,
    Button,
    ButtonGroup,
    Center,
    Divider,
    Flex,
    Heading,
    HStack,
    Image,
    Link,
    Skeleton,
    Stack,
    Text,
    Tooltip,
    useColorMode,
    useColorModeValue,
    VStack,
} from "@chakra-ui/react";
import * as React from "react";
import { FaHome, FaWallet } from "react-icons/all";
import { Link as ReachLink, useLocation, useNavigate } from "react-router-dom";
import { BG_DARK, BG_LIGHT, HEADER_HEIGHT, VIEWPORT_WIDTH_PX } from "../App";
import logoBlack from "../images/logo_nav_bar_black.svg";
import logoWhite from "../images/logo_nav_bar_white.svg";
import { ConnectionStatus, WalletInfo } from "../types";
import DollarAmount from "./DollarAmount";

interface NavProps {
    walletInfo: WalletInfo | null;
    connectedToMaker: ConnectionStatus;
    nextFundingEvent: string | null;
    referencePrice: number | undefined;
}

function TextDivider() {
    return <Divider orientation={"vertical"} borderColor={useColorModeValue("black", "white")} height={"20px"} />;
}

export default function Nav({ walletInfo, connectedToMaker, nextFundingEvent, referencePrice }: NavProps) {
    const navigate = useNavigate();

    const { toggleColorMode } = useColorMode();
    const navBarLog = useColorModeValue(
        <Image src={logoBlack} w="128px" />,
        <Image src={logoWhite} w="128px" />,
    );

    const toggleIcon = useColorModeValue(
        <MoonIcon />,
        <SunIcon />,
    );

    const connectionStatus = (connectedToMaker: ConnectionStatus) => {
        if (connectedToMaker.online) {
            return {
                warn: false,
                light: "green.600",
                dark: "green.400",
                tooltip: "The maker is online",
            };
        }

        return {
            warn: false,
            light: "red.600",
            dark: "red.400",
            tooltip: "The maker is offline",
        };
    };

    const connectionStatusDisplay = connectionStatus(connectedToMaker);

    const connectionStatusIconColor = useColorModeValue(
        connectionStatusDisplay.light,
        connectionStatusDisplay.dark,
    );

    return (
        <>
            <VStack spacing={0} position={"fixed"} width={"100%"} zIndex={"100"} height={`${HEADER_HEIGHT}px`}>
                <Box bg={useColorModeValue("gray.100", "gray.900")} width={"100%"}>
                    <Center>
                        <Box
                            paddingBottom={2}
                            bg={useColorModeValue("gray.100", "gray.900")}
                            width={"100%"}
                            maxWidth={VIEWPORT_WIDTH_PX}
                        >
                            <Flex alignItems={"center"} justifyContent={"space-between"} padding={2}>
                                <Flex justifyContent={"flex-left"}>
                                    <NavigationButtons />
                                </Flex>
                                <Flex alignItems={"center"}>
                                    <Stack direction={"row"} spacing={7}>
                                        <Button onClick={toggleColorMode} variant={"unstyled"}>
                                            {toggleIcon}
                                        </Button>
                                        <Box>
                                            <Button bg={"transparent"} onClick={() => navigate("/")}>
                                                {navBarLog}
                                            </Button>
                                        </Box>
                                    </Stack>
                                </Flex>
                            </Flex>
                        </Box>
                    </Center>
                </Box>

                <Box
                    paddingTop={2}
                    bg={useColorModeValue(BG_LIGHT, BG_DARK)}
                    width={"100%"}
                    maxWidth={VIEWPORT_WIDTH_PX}
                    height={"100%"}
                >
                    <Center>
                        <HStack>
                            <Tooltip label={connectionStatusDisplay.tooltip}>
                                <HStack>
                                    <Text>{"Maker "}</Text>
                                    {connectionStatusDisplay.warn
                                        ? (
                                            <WarningIcon
                                                color={connectionStatusIconColor}
                                            />
                                        )
                                        : (
                                            <Icon
                                                viewBox="0 0 200 200"
                                                color={connectionStatusIconColor}
                                            >
                                                <path
                                                    fill="currentColor"
                                                    d="M 100, 100 m -75, 0 a 75,75 0 1,0 150,0 a 75,75 0 1,0 -150,0"
                                                />
                                            </Icon>
                                        )}
                                </HStack>
                            </Tooltip>
                            <TextDivider />
                            <Text>{"Funding "}</Text>
                            <Skeleton
                                isLoaded={nextFundingEvent != null}
                                height={"20px"}
                                display={"flex"}
                                alignItems={"center"}
                            >
                                <Tooltip
                                    label={"The next time your CFDs will be extended and the funding fee will be collected based on the hourly rate."}
                                    hasArrow
                                >
                                    <HStack minWidth={"80px"} maxWidth={["90px", "90px", "inherit"]}>
                                        <Heading
                                            size={"sm"}
                                            textOverflow={"ellipsis"}
                                            overflow={"hidden"}
                                            whiteSpace={"nowrap"}
                                        >
                                            {nextFundingEvent}
                                        </Heading>
                                    </HStack>
                                </Tooltip>
                            </Skeleton>
                            <TextDivider />
                            <Text display={["inherit", "inherit", "none"]}>Ref</Text>
                            <Text display={["none", "none", "inherit"]}>Reference Price</Text>
                            <Skeleton
                                isLoaded={referencePrice !== undefined}
                                height={"20px"}
                                display={"flex"}
                                alignItems={"center"}
                            >
                                <Tooltip
                                    label={"The price the Oracle attests to, the BitMEX BXBT index price"}
                                    hasArrow
                                >
                                    <Link href={"https://outcome.observer/h00.ooo/x/BitMEX/BXBT"} target={"_blank"}>
                                        {/* The minWidth helps with not letting the elements in Nav jump because the width changes*/}
                                        <Heading size={"sm"} minWidth={"90px"}>
                                            <DollarAmount amount={referencePrice || 0} />
                                        </Heading>
                                    </Link>
                                </Tooltip>
                            </Skeleton>
                        </HStack>
                    </Center>
                </Box>
            </VStack>
        </>
    );
}

function NavigationButtons() {
    const location = useLocation();
    const isWalletSelected = location.pathname.includes("wallet");
    const isHomeSelected = !isWalletSelected;

    const unSelectedButton = "transparent";
    const selectedButton = useColorModeValue("grey.400", "black.400");
    const buttonBorder = useColorModeValue("grey.400", "black.400");
    const buttonText = useColorModeValue("black", "white");

    const width = "100px";
    const borderWidth = 2;

    return (
        <HStack>
            <Center>
                <ButtonGroup spacing="3">
                    <Button
                        as={ReachLink}
                        to="/long"
                        color={isHomeSelected ? selectedButton : unSelectedButton}
                        bg={isHomeSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        borderWidth={borderWidth}
                        isActive={isHomeSelected}
                        w={width}
                        leftIcon={<FaHome color={buttonText} />}
                        boxShadow={"md"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Home</Text>
                    </Button>
                    <Button
                        as={ReachLink}
                        to="/wallet"
                        color={isWalletSelected ? selectedButton : unSelectedButton}
                        bg={isWalletSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        borderWidth={borderWidth}
                        isActive={isWalletSelected}
                        w={width}
                        leftIcon={<FaWallet color={buttonText} />}
                        boxShadow={"md"}
                        id={"walletSwitchButton"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Wallet</Text>
                    </Button>
                </ButtonGroup>
            </Center>
        </HStack>
    );
}
